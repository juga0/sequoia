//! Storage backend.

use failure;
use std::cmp;
use std::fmt;
use std::io;
use std::rc::Rc;
use time::Duration;

use capnp::capability::Promise;
use capnp;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::{self, RpcSystem, twoparty};
use futures::Future;
use futures::future::{self, loop_fn, Loop};
use rand::distributions::{Distribution, Uniform};
use rand::thread_rng;
use rusqlite::Connection;
use rusqlite;
use tokio_core::reactor::{Handle, Timeout};
use tokio_core;
use tokio_io::io::ReadHalf;

use openpgp::{self, TPK, KeyID, Fingerprint};
use openpgp::parse::Parse;
use openpgp::serialize::Serialize;
use sequoia_core as core;
use sequoia_net as net;
use sequoia_ipc as ipc;

use store_protocol_capnp::node;

use super::Result;

// Data types for working with `rusqlite`.
pub mod support;
use self::support::{ID, Timestamp};

// Logging.
mod log;

/* Configuration and policy.  */

/// Minimum sleep time.
fn min_sleep_time() -> Duration {
    Duration::minutes(5)
}

/// Interval after which all keys should be refreshed once.
fn refresh_interval() -> Duration {
    Duration::weeks(1)
}

/// Returns a value from the uniform distribution over [0, 2*d).
///
/// This function is used to randomize key refresh times.
fn random_duration(d: Duration) -> Duration {
    let s = Uniform::from(0..2 * d.num_seconds())
        .sample(&mut thread_rng());
    Duration::seconds(s)
}

/* Entry point.  */

/// Makes backends.
pub fn factory(descriptor: ipc::Descriptor, handle: Handle)
               -> Result<Box<ipc::Handler>> {
    Backend::new(descriptor, handle)
        .map(|b| -> Box<ipc::Handler> { Box::new(b) })
}

struct Backend {
    store: node::Client,
}

impl Backend {
    fn new(descriptor: ipc::Descriptor, handle: Handle) -> Result<Self> {
        Ok(Backend {
            store: node::ToClient::new(NodeServer::new(descriptor, handle)?)
                .into_client::<capnp_rpc::Server>(),
        })
    }
}

impl ipc::Handler for Backend {
    fn handle(&self,
              network: twoparty::VatNetwork<ReadHalf<tokio_core::net::TcpStream>>)
              -> RpcSystem<Side> {
        RpcSystem::new(Box::new(network), Some(self.store.clone().client))
    }
}

/* Server implementation.  */

struct NodeServer {
    _descriptor: ipc::Descriptor,
    c: Rc<Connection>,
}

impl NodeServer {
    fn new(descriptor: ipc::Descriptor, handle: Handle) -> Result<Self> {
        let mut db_path = descriptor.context().home().to_path_buf();
        db_path.push("public-key-store.sqlite");

        let c = Connection::open(db_path)?;
        c.execute_batch("PRAGMA secure_delete = true;")?;
        c.execute_batch("PRAGMA foreign_keys = true;")?;
        let server = NodeServer {
            _descriptor: descriptor,
            c: Rc::new(c),
        };
        server.init()?;

        KeyServer::start_housekeeping(server.c.clone(), handle)?;
        Ok(server)
    }

    /// Initializes or migrates the database.
    fn init(&self) -> Result<()> {
        let v = self.c.query_row(
            "SELECT version FROM version WHERE id=1",
            &[], |row| row.get(0));

        if let Ok(v) = v {
            match v {
                1 => return Ok(()),
                _ => unimplemented!(),
            }
        }

        self.c.execute_batch(DB_SCHEMA_1)?;
        log::message(&self.c, log::Refers::to(), "server",
                     "Created database version 1")?;
        Ok(())
    }
}

impl node::Server for NodeServer {
    fn open(&mut self,
            params: node::OpenParams,
            mut results: node::OpenResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let params = pry!(params.get());

        // XXX maybe check ephemeral and use in-core sqlite db

        let store = sry!(StoreServer::open(self.c.clone(),
                                           pry!(params.get_realm()),
                                           pry!(params.get_network_policy()).into(),
                                           pry!(params.get_name())));
        pry!(pry!(results.get().get_result()).set_ok(
            node::store::ToClient::new(store).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn iter(&mut self,
            params: node::IterParams,
            mut results: node::IterResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let prefix = pry!(pry!(params.get()).get_realm_prefix());
        let iter = StoreIterServer::new(self.c.clone(), prefix);
        pry!(pry!(results.get().get_result()).set_ok(
            node::store_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn iter_keys(&mut self,
                 _: node::IterKeysParams,
                 mut results: node::IterKeysResults)
                 -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = KeyIterServer::new(self.c.clone());
        pry!(pry!(results.get().get_result()).set_ok(
            node::key_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn log(&mut self,
           _: node::LogParams,
           mut results: node::LogResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = log::IterServer::new(self.c.clone(), log::Selector::All);
        pry!(pry!(results.get().get_result()).set_ok(
            node::log_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn import(&mut self,
              params: node::ImportParams,
              mut results: node::ImportResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        let new = sry!(TPK::from_bytes(&pry!(pry!(params.get()).get_key())));
        let fp = new.fingerprint();
        let key_id = sry!(KeyServer::lookup_or_create(&self.c, &fp));
        let key = KeyServer::new(self.c.clone(), key_id);
        sry!(key.merge(new));
        pry!(pry!(results.get().get_result())
             .set_ok(node::key::ToClient::new(key)
                     .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn lookup_by_keyid(&mut self,
                       params: node::LookupByKeyidParams,
                       mut results: node::LookupByKeyidResults)
                       -> Promise<(), capnp::Error> {
        bind_results!(results);
        let keyid = pry!(params.get()).get_keyid();
        let keyid = KeyID::new(keyid);
        let key_id = sry!(KeyServer::lookup_by_id(&self.c, &keyid));

        pry!(pry!(results.get().get_result()).set_ok(
            node::key::ToClient::new(
                KeyServer::new(self.c.clone(), key_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn lookup_by_fingerprint(&mut self,
                             params: node::LookupByFingerprintParams,
                             mut results: node::LookupByFingerprintResults)
                             -> Promise<(), capnp::Error> {
        bind_results!(results);
        let fingerprint = pry!(pry!(params.get()).get_fingerprint());
        let fingerprint = sry!(Fingerprint::from_hex(fingerprint));
        let key_id = sry!(KeyServer::lookup(&self.c, &fingerprint));

        pry!(pry!(results.get().get_result()).set_ok(
            node::key::ToClient::new(
                KeyServer::new(self.c.clone(), key_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn lookup_by_subkeyid(&mut self,
                          params: node::LookupBySubkeyidParams,
                          mut results: node::LookupBySubkeyidResults)
                          -> Promise<(), capnp::Error> {
        bind_results!(results);
        let keyid = pry!(params.get()).get_keyid();

        let key_id: ID = sry!(
            self.c.query_row(
                "SELECT key FROM key_by_keyid
                 WHERE key_by_keyid.keyid = ?1",
                &[&(keyid as i64)], |row| row.get(0)));

        pry!(pry!(results.get().get_result()).set_ok(
            node::key::ToClient::new(
                KeyServer::new(self.c.clone(), key_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }
}

struct StoreServer {
    c: Rc<Connection>,
    id: ID,
}

impl Query for StoreServer {
    fn table_name() -> &'static str {
        "stores"
    }

    fn id(&self) -> ID {
        self.id
    }

    fn connection(&self) -> Rc<Connection> {
        self.c.clone()
    }

    fn slug(&self) -> String {
        self.c.query_row(
            "SELECT realm, name FROM stores WHERE id = ?1",
            &[&self.id], |row| -> String {
                format!("{}:{}", row.get::<_, String>(0), row.get::<_, String>(1))
            })
            .unwrap_or(
                format!("{}::{}", Self::table_name(), self.id())
            )
    }
}

impl StoreServer {
    fn new(c: Rc<Connection>, id: ID) -> StoreServer {
        StoreServer{c: c, id: id}
    }

    fn open(c: Rc<Connection>, realm: &str, policy: core::NetworkPolicy, name: &str)
           -> Result<Self> {
        // We cannot implement ToSql and friends for
        // core::NetworkPolicy, hence we need to do it by foot.
        let p: u8 = (&policy).into();

        c.execute(
            "INSERT OR IGNORE INTO stores (realm, network_policy, name) VALUES (?1, ?2, ?3)",
            &[&realm, &p, &name])?;
        let (id, store_policy): (ID, i64) = c.query_row(
            "SELECT id, network_policy FROM stores WHERE realm = ?1 AND name = ?2",
            &[&realm, &name], |row| (row.get(0), row.get(1)))?;

        // We cannot implement FromSql and friends for
        // core::NetworkPolicy, hence we need to do it by foot.
        if store_policy < 0 || store_policy > 3 {
            return Err(super::Error::ProtocolError.into());
        }
        let store_policy = core::NetworkPolicy::from(store_policy as u8);

        if store_policy != policy {
            return Err(core::Error::NetworkPolicyViolation(store_policy)
                       .into());
        }

        Ok(Self::new(c, id))
    }
}

impl node::store::Server for StoreServer {
    fn add(&mut self,
           params: node::store::AddParams,
           mut results: node::store::AddResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let params = pry!(params.get());
        let fp = pry!(params.get_fingerprint());
        let fp = sry!(Fingerprint::from_hex(fp)
                      .map_err(|_| node::Error::MalformedFingerprint));
        let label = pry!(params.get_label());

        let (binding_id, key_id, created) = sry!(
            BindingServer::lookup_or_create(&self.c, self.id, label, &fp));

        if created {
            sry!(log::message(
                &self.c,
                log::Refers::to().store(self.id).binding(binding_id).key(key_id),
                &self.slug(),
                &format!("New binding {} -> {}", label, fp.to_keyid())));
        }


        pry!(pry!(results.get().get_result()).set_ok(
            node::binding::ToClient::new(
                BindingServer::new(self.c.clone(), binding_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn lookup(&mut self,
              params: node::store::LookupParams,
              mut results: node::store::LookupResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        let label = pry!(pry!(params.get()).get_label());

        let binding_id: ID = sry!(
            self.c.query_row(
                "SELECT id FROM bindings WHERE store = ?1 AND label = ?2",
                &[&self.id, &label], |row| row.get(0)));

        pry!(pry!(results.get().get_result()).set_ok(
            node::binding::ToClient::new(
                BindingServer::new(self.c.clone(), binding_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn lookup_by_subkeyid(&mut self,
                          params: node::store::LookupBySubkeyidParams,
                          mut results: node::store::LookupBySubkeyidResults)
                          -> Promise<(), capnp::Error> {
        bind_results!(results);
        let keyid = pry!(params.get()).get_keyid();

        let binding_id: ID = sry!(
            self.c.query_row(
                "SELECT bindings.id FROM bindings
                 JOIN key_by_keyid on bindings.key = key_by_keyid.key
                 WHERE key_by_keyid.keyid = ?1",
                &[&(keyid as i64)], |row| row.get(0)));

        pry!(pry!(results.get().get_result()).set_ok(
            node::binding::ToClient::new(
                BindingServer::new(self.c.clone(), binding_id))
                .into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn delete(&mut self,
              _: node::store::DeleteParams,
              mut results: node::store::DeleteResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        sry!(self.c.execute("DELETE FROM stores WHERE id = ?1",
                                     &[&self.id]));
        Promise::ok(())
    }

    fn iter(&mut self,
            _: node::store::IterParams,
            mut results: node::store::IterResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = BindingIterServer::new(self.c.clone(), self.id);
        pry!(pry!(results.get().get_result()).set_ok(
            node::binding_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn log(&mut self,
           _: node::store::LogParams,
           mut results: node::store::LogResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = log::IterServer::new(self.c.clone(), log::Selector::Store(self.id));
        pry!(pry!(results.get().get_result()).set_ok(
            node::log_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }
}

struct BindingServer {
    c: Rc<Connection>,
    id: ID,
}

impl BindingServer {
    fn new(c: Rc<Connection>, id: ID) -> Self {
        BindingServer {
            c: c,
            id: id,
        }
    }

    fn key_id(&mut self) -> Result<ID> {
        self.query("key").map(|id| id.into())
    }


    /// Looks up a binding, creating a binding if necessary.
    ///
    /// On success, the id of the binding and the key is returned, and
    /// whether or not the entry was just created.
    fn lookup_or_create(c: &Connection, store: ID, label: &str, fp: &Fingerprint)
                        -> Result<(ID, ID, bool)> {
        let key_id = KeyServer::lookup_or_create(c, fp)?;
        if let Ok((binding, key)) = c.query_row(
            "SELECT id, key FROM bindings WHERE store = ?1 AND label = ?2",
            &[&store, &label], |row| -> (ID, ID) {(row.get(0), row.get(1))}) {
            if key == key_id {
                Ok((binding, key_id, false))
            } else {
                Err(super::Error::Conflict.into())
            }
        } else {
            let r = c.execute(
                "INSERT INTO bindings (store, label, key, created)
                 VALUES (?, ?, ?, ?)",
                &[&store, &label, &key_id, &Timestamp::now()]);

            // Some other mutator might race us to the insertion.
            match r {
                Err(rusqlite::Error::SqliteFailure(f, _)) => match f.code {
                    // We lost.  Retry the lookup.
                    rusqlite::ErrorCode::ConstraintViolation => {
                        let (binding, key): (ID, ID) = c.query_row(
                            "SELECT id, key FROM bindings WHERE store = ?1 AND label = ?2",
                            &[&store, &label], |row| (row.get(0), row.get(1)))?;
                        if key == key_id {
                            Ok((binding, key_id, false))
                        } else {
                            Err(node::Error::Conflict)
                        }
                    },
                    // Raise otherwise.
                    _ => Err(node::Error::SystemError),
                },
                Err(_) => Err(node::Error::SystemError),
                Ok(_) => Ok((ID::from(c.last_insert_rowid()), key_id, true)),
            }.map_err(|e| e.into())
        }
    }
}

impl Query for BindingServer {
    fn table_name() -> &'static str {
        "bindings"
    }

    fn id(&self) -> ID {
        self.id
    }

    fn connection(&self) -> Rc<Connection> {
        self.c.clone()
    }

    fn slug(&self) -> String {
        self.c.query_row(
            "SELECT label FROM bindings WHERE id = ?1",
            &[&self.id], |row| -> String {
                row.get(0)
            })
            .unwrap_or(
                format!("{}::{}", Self::table_name(), self.id())
            )
    }
}

impl node::binding::Server for BindingServer {
    fn stats(&mut self,
             _: node::binding::StatsParams,
             mut results: node::binding::StatsResults)
             -> Promise<(), capnp::Error> {
        bind_results!(results);
        sry!(self.query_stats(pry!(results.get().get_result()).init_ok()));
        Promise::ok(())
    }

    fn key(&mut self,
           _: node::binding::KeyParams,
           mut results: node::binding::KeyResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let key = sry!(self.key_id());

        pry!(pry!(results.get().get_result()).set_ok(
            node::key::ToClient::new(
                KeyServer::new(self.c.clone(), key)).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn import(&mut self,
              params: node::binding::ImportParams,
              mut results: node::binding::ImportResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        let force = pry!(params.get()).get_force();

        // This is the key to import.
        let mut new = sry!(TPK::from_bytes(&pry!(pry!(params.get()).get_key())));

        // Check in the database for the current key.
        let key_id = sry!(self.key_id());
        let (fingerprint, key): (String, Option<Vec<u8>>)
            = sry!(self.c.query_row(
                "SELECT fingerprint, key FROM keys WHERE id = ?1",
                &[&key_id],
                |row| (row.get(0), row.get_checked(1).ok())));

        // If we found one, convert it to TPK.
        let current = if let Some(current) = key {
            let current = sry!(TPK::from_bytes(&current));
            if current.fingerprint().to_hex() != fingerprint {
                // Inconsistent database.
                fail!(node::Error::SystemError);
            }
            Some(current)
        } else {
            None
        };

        // Check for conflicts.
        if new.fingerprint().to_hex() != fingerprint {
            if force {
                // Update binding, and retry.
                let key_id =
                    sry!(KeyServer::lookup_or_create(
                        &self.c, &new.fingerprint()));
                sry!(self.c.execute("UPDATE bindings SET key = ?1 WHERE id = ?2",
                                    &[&key_id, &self.id]));
                return self.import(params, results);
            } else {
                fail!(node::Error::Conflict);
            }
        }

        if current.is_some() {
            new = sry!(current.unwrap().merge(new));
        }

        // Write key back to the database.
        let mut blob = vec![];
        sry!(new.serialize(&mut blob));

        sry!(self.c.execute("UPDATE keys SET key = ?1 WHERE id = ?2",
                            &[&blob, &key_id]));
        sry!(KeyServer::reindex_subkeys(&self.c, key_id, &new));

        pry!(pry!(results.get().get_result()).set_ok(&blob[..]));
        Promise::ok(())
    }

    fn delete(&mut self,
              _: node::binding::DeleteParams,
              mut results: node::binding::DeleteResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        sry!(self.c.execute("DELETE FROM bindings WHERE id = ?1",
                                     &[&self.id]));
        Promise::ok(())
    }

    fn register_encryption(&mut self,
                           _: node::binding::RegisterEncryptionParams,
                           mut results: node::binding::RegisterEncryptionResults)
                           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let now = Timestamp::now();
        let key = sry!(self.key_id());

        sry!(self.c
             .execute("UPDATE bindings
                       SET encryption_count = encryption_count + 1,
                           encryption_first = coalesce(encryption_first, ?2),
                           encryption_last = ?2
                       WHERE id = ?1",
                      &[&self.id, &now]));
        sry!(self.c
             .execute("UPDATE keys
                       SET encryption_count = encryption_count + 1,
                           encryption_first = coalesce(encryption_first, ?2),
                           encryption_last = ?2
                       WHERE id = ?1",
                      &[&key, &now]));

        sry!(self.query_stats( pry!(results.get().get_result()).init_ok()));
        Promise::ok(())
    }

    fn register_verification(&mut self,
                             _: node::binding::RegisterVerificationParams,
                             mut results: node::binding::RegisterVerificationResults)
                             -> Promise<(), capnp::Error> {
        bind_results!(results);
        let now = Timestamp::now();
        let key = sry!(self.key_id());

        sry!(self.c
             .execute("UPDATE bindings
                       SET verification_count = verification_count + 1,
                           verification_first = coalesce(verification_first, ?2),
                           verification_last = ?2
                       WHERE id = ?1",
                      &[&self.id, &now]));
        sry!(self.c
             .execute("UPDATE keys
                       SET verification_count = verification_count + 1,
                           verification_first = coalesce(verification_first, ?2),
                           verification_last = ?2
                       WHERE id = ?1",
                      &[&key, &now]));

        sry!(self.query_stats( pry!(results.get().get_result()).init_ok()));
        Promise::ok(())
    }

    fn log(&mut self,
           _: node::binding::LogParams,
           mut results: node::binding::LogResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = log::IterServer::new(self.c.clone(), log::Selector::Binding(self.id));
        pry!(pry!(results.get().get_result()).set_ok(
            node::log_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }

    fn label(&mut self,
           _: node::binding::LabelParams,
           mut results: node::binding::LabelResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let label = sry!(self.c.query_row(
            "SELECT label FROM bindings WHERE id = ?1",
            &[&self.id], |row| -> String {
                row.get(0)
            }));

        pry!(pry!(results.get().get_result()).set_ok(label.as_str()));
        Promise::ok(())
    }
}

struct KeyServer {
    c: Rc<Connection>,
    id: ID,
}

impl KeyServer {
    fn new(c: Rc<Connection>, id: ID) -> Self {
        KeyServer {
            c: c,
            id: id,
        }
    }

    /// Looks up a key by fingerprint.
    ///
    /// On success, the id of the key is returned.
    fn lookup(c: &Connection, fp: &Fingerprint) -> Result<ID> {
        let fp = fp.to_hex();
        Ok(c.query_row(
            "SELECT id FROM keys WHERE fingerprint = ?1",
            &[&fp], |row| row.get(0))?)
    }

    /// Looks up a key by keyid.
    ///
    /// On success, the id of the key is returned.
    fn lookup_by_id(c: &Connection, keyid: &KeyID) -> Result<ID> {
        let keyid = format!("%{}", keyid.to_hex());
        Ok(c.query_row(
            "SELECT id FROM keys WHERE fingerprint LIKE ?1",
            &[&keyid], |row| row.get(0))?)
    }

    /// Looks up a fingerprint, creating a key if necessary.
    ///
    /// On success, the id of the key is returned.
    fn lookup_or_create(c: &Connection, fp: &Fingerprint) -> Result<ID> {
        let fp = fp.to_hex();
        if let Ok(x) = c.query_row(
            "SELECT id FROM keys WHERE fingerprint = ?1",
            &[&fp], |row| row.get(0)) {
            Ok(x)
        } else {
            let r = c.execute(
                "INSERT INTO keys (fingerprint, created, update_at) VALUES (?1, ?2, ?2)",
                &[&fp, &Timestamp::now()]);

            // Some other mutator might race us to the insertion.
            match r {
                Err(rusqlite::Error::SqliteFailure(f, e)) => match f.code {
                    // We lost.  Retry the lookup.
                    rusqlite::ErrorCode::ConstraintViolation =>
                        c.query_row(
                            "SELECT id FROM keys WHERE fingerprint = ?1",
                            &[&fp], |row| row.get(0)),
                    // Raise otherwise.
                    _ => Err(rusqlite::Error::SqliteFailure(f, e)),
                },
                Err(e) => Err(e),
                Ok(_) => Ok(c.last_insert_rowid().into()),
            }.map_err(|e| e.into())
        }
    }

    /// Merges other into this key updating the database.
    ///
    /// Returnes the merged key as blob.
    fn merge(&self, other: TPK) -> Result<Vec<u8>> {
        let mut new = other;

        // Get the current key from the database.
        let (fingerprint, key): (String, Option<Vec<u8>>)
            = self.c.query_row(
                "SELECT fingerprint, key FROM keys WHERE id = ?1",
                &[&self.id],
                |row| (row.get(0), row.get_checked(1).ok()))?;

        // If there was a key stored there, merge it.
        if let Some(current) = key {
            let current = TPK::from_bytes(&current)?;

            if current.fingerprint().to_hex() != fingerprint {
                // Inconsistent database.
                return Err(node::Error::SystemError.into());
            }

            if current.fingerprint() != new.fingerprint() {
                return Err(node::Error::Conflict.into());
            }

            new = current.merge(new)?;
        }

        // Write key back to the database.
        let mut blob = vec![];
        new.serialize(&mut blob)?;

        self.c.execute("UPDATE keys SET key = ?1 WHERE id = ?2",
                       &[&blob, &self.id])?;
        KeyServer::reindex_subkeys(&self.c, self.id, &new)?;

        Ok(blob)
    }

    /// Keeps the mapping of (sub)KeyIDs to keys up-to-date.
    fn reindex_subkeys(c: &Connection, key_id: ID, tpk: &TPK) -> Result<()> {
        for (_, _, key) in tpk.keys_all() {
            let keyid = key.keyid().as_u64()
                .expect("computed keyid is valid");

            let r = c.execute(
                "INSERT INTO key_by_keyid (keyid, key) VALUES (?1, ?2)",
                &[&(keyid as i64), &key_id]);

            // The mapping might already be present.  This is not an error.
            match r {
                Err(rusqlite::Error::SqliteFailure(f, e)) => match f.code {
                    // Already present.
                    rusqlite::ErrorCode::ConstraintViolation =>
                        Ok(()),
                    // Raise otherwise.
                    _ => Err(rusqlite::Error::SqliteFailure(f, e)),
                },
                Err(e) => Err(e),
                Ok(_) => Ok(()),
            }?;
        }
        Ok(())
    }

    /// Records a successful key update.
    fn success(&self, message: &str, next: Duration) -> Result<()> {
        log::message(&self.c, log::Refers::to().key(self.id),
                     &self.slug(), message)?;
        self.c.execute("UPDATE keys
                        SET updated = ?2, update_at = ?3
                        WHERE id = ?1",
                       &[&self.id, &Timestamp::now(),
                         &(Timestamp::now() + next)])?;
        Ok(())
    }

    /// Records an unsuccessful key update.
    fn error(&self, message: &str, error: &str, next: Duration) -> Result<()> {
        log::error(&self.c, log::Refers::to().key(self.id),
                   &self.slug(), message, error)?;
        self.c.execute("UPDATE keys
                        SET update_at = ?2
                        WHERE id = ?1",
                       &[&self.id,
                         &(Timestamp::now() + next)])?;
        Ok(())
    }

    /// Returns when the next key using the given policy should be updated.
    fn next_update_at(c: &Rc<Connection>, network_policy: core::NetworkPolicy)
                      -> Option<Timestamp> {
        let network_policy_u8 = u8::from(&network_policy);

        // Select the key that was updated least recently.
        c.query_row(
            "SELECT keys.update_at FROM keys
                 JOIN bindings on keys.id = bindings.key
                 JOIN stores on stores.id = bindings.store
                 WHERE stores.network_policy = ?1
                 ORDER BY keys.update_at LIMIT 1",
            &[&network_policy_u8], |row| -> Timestamp {row.get(0)}).ok()
    }

    /// Returns the number of keys using the given policy.
    fn need_update(c: &Rc<Connection>, network_policy: core::NetworkPolicy)
                   -> Result<i32> {
        let network_policy_u8 = u8::from(&network_policy);

        let count: i64 = c.query_row(
            "SELECT COUNT(*) FROM keys
                 JOIN bindings on keys.id = bindings.key
                 JOIN stores on stores.id = bindings.store
                 WHERE stores.network_policy >= ?1",
            &[&network_policy_u8], |row| row.get(0))?;
        assert!(count >= 0);
        Ok(count as i32)
    }

    /// Helper for `update`.
    fn update_helper(c: &Rc<Connection>,
                     network_policy: core::NetworkPolicy)
                     -> Result<(KeyServer,
                                openpgp::KeyID,
                                net::async::KeyServer)> {
        assert!(network_policy != core::NetworkPolicy::Offline);
        let network_policy_u8 = u8::from(&network_policy);

        // Select the key that was updated least recently.
        let (id, fingerprint): (ID, String) = c.query_row(
            "SELECT keys.id, keys.fingerprint FROM keys
                 JOIN bindings on keys.id = bindings.key
                 JOIN stores on stores.id = bindings.store
                 WHERE stores.network_policy >= ?1
                   AND keys.update_at < ?2
                 ORDER BY keys.update_at LIMIT 1",
            &[&network_policy_u8, &Timestamp::now()], |row| (row.get(0),
                                                             row.get(1)))?;
        let fingerprint = openpgp::Fingerprint::from_hex(&fingerprint)
            .map_err(|_| node::Error::SystemError)?;

        let ctx = core::Context::configure()
            .network_policy(network_policy).build()?;
        let keyserver = net::async::KeyServer::sks_pool(&ctx)?;

        Ok((KeyServer::new(c.clone(), id),
            fingerprint.to_keyid(),
            keyserver))
    }

    /// Updates the key that was least recently updated.
    fn update(c: &Rc<Connection>,
              network_policy: core::NetworkPolicy)
              -> Box<Future<Item=Duration, Error=failure::Error> + 'static> {
        let (key, id, mut keyserver)
            = match Self::update_helper(c, network_policy) {
            Ok((key, id, keyserver)) => (key, id, keyserver),
            Err(e) => return Box::new(future::err(e.into())),
        };

        let c = c.clone();
        let now = Timestamp::now();
        let at = Self::next_update_at(&c, network_policy)
            .unwrap_or(now + min_sleep_time());

        if at <= now {
            Box::new(
                keyserver.get(&id)
                    .then(move |tpk| {
                        let next = Self::need_update(&c, network_policy)
                            .map(|c| refresh_interval() / c)
                            .unwrap_or(min_sleep_time());

                        if let Err(e) = tpk.map(|t| key.merge(t)) {
                            key.error("Update unsuccessful",
                                      &format!("{:?}", e), next / 2)
                                .unwrap_or(());
                        } else {
                            key.success("Update successful", next)
                                .unwrap_or(());
                        }

                        future::ok(next)
                    }))
        } else {
            assert!(at > now);
            Box::new(future::ok(cmp::max(min_sleep_time(), at - now)))
        }
    }

    /// Starts the periodic housekeeping.
    fn start_housekeeping(c: Rc<Connection>, handle: Handle) -> Result<()> {
        let h0 = handle.clone();

        let forever = loop_fn(0, move |_| {
            // For now, we only update keys with this network policy.
            let network_policy = core::NetworkPolicy::Encrypted;

            let h1 = h0.clone();
            Self::update(&c, network_policy)
                .then(move |d| {
                    let d = d.unwrap_or(min_sleep_time());
                     Timeout::new(
                         ::std::time::Duration::new(random_duration(d)
                                                    .num_seconds() as u64, 0),
                         &h1)
                     .unwrap() // XXX: May fail if the eventloop expired.
                     .then(move |timeout| {
                         if timeout.is_ok() {
                             Ok(Loop::Continue(0))
                         } else {
                             Ok(Loop::Break(()))
                         }
                     })
                })
        });
        handle.spawn(forever);
        Ok(())
    }
}

impl Query for KeyServer {
    fn table_name() -> &'static str {
        "keys"
    }

    fn id(&self) -> ID {
        self.id
    }

    fn connection(&self) -> Rc<Connection> {
        self.c.clone()
    }

    fn slug(&self) -> String {
        self.c.query_row(
            "SELECT fingerprint FROM keys WHERE id = ?1",
            &[&self.id], |row| -> String { row.get(0) })
            .ok()
            .and_then(|fp| Fingerprint::from_hex(&fp).ok())
            .map(|fp| fp.to_keyid().to_string())
            .unwrap_or(
                format!("{}::{}", Self::table_name(), self.id())
            )
    }
}

impl node::key::Server for KeyServer {
    fn stats(&mut self,
             _: node::key::StatsParams,
             mut results: node::key::StatsResults)
             -> Promise<(), capnp::Error> {
        bind_results!(results);
        sry!(self.query_stats( pry!(results.get().get_result()).init_ok()));
        Promise::ok(())
    }

    fn tpk(&mut self,
           _: node::key::TpkParams,
           mut results: node::key::TpkResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let key: Vec<u8> = sry!(
            self.c.query_row(
                "SELECT key FROM keys WHERE id = ?1",
                &[&self.id],
                |row| row.get_checked(0).unwrap_or(vec![])));
        pry!(pry!(results.get().get_result()).set_ok(key.as_slice()));
        Promise::ok(())
    }

    fn import(&mut self,
              params: node::key::ImportParams,
              mut results: node::key::ImportResults)
              -> Promise<(), capnp::Error> {
        bind_results!(results);
        let new = sry!(TPK::from_bytes(&pry!(pry!(params.get()).get_key())));
        let blob = sry!(self.merge(new));
        pry!(pry!(results.get().get_result()).set_ok(&blob[..]));
        Promise::ok(())
    }

    fn log(&mut self,
           _: node::key::LogParams,
           mut results: node::key::LogResults)
           -> Promise<(), capnp::Error> {
        bind_results!(results);
        let iter = log::IterServer::new(self.c.clone(), log::Selector::Key(self.id));
        pry!(pry!(results.get().get_result()).set_ok(
            node::log_iter::ToClient::new(iter).into_client::<capnp_rpc::Server>()));
        Promise::ok(())
    }
}

/// Common code for BindingServer and KeyServer.
trait Query {
    fn table_name() -> &'static str;
    fn id(&self) -> ID;
    fn connection(&self) -> Rc<Connection>;
    fn slug(&self) -> String;

    fn query(&mut self, column: &str) -> Result<i64> {
        self.connection().query_row(
            &format!("SELECT {} FROM {} WHERE id = ?1", column, Self::table_name()),
            &[&self.id()], |row| row.get(0)).map_err(|e| e.into())
    }

    fn query_stats(&mut self, mut stats: node::stats::Builder) -> Result<()> {
        let (
            created, updated,
            encryption_count, encryption_first, encryption_last,
            verification_count, verification_first, verification_last,
        ): (i64, Option<i64>,
            i64, Option<i64>, Option<i64>,
            i64, Option<i64>, Option<i64>)
            = self.connection().query_row(
                &format!("SELECT
                          created,
                          updated,
                          encryption_count,
                          encryption_first,
                          encryption_last,
                          verification_count,
                          verification_first,
                          verification_last
                          FROM {0}
                          WHERE id = ?1", Self::table_name()),
                &[&self.id()], |row| (row.get(0), row.get(1),
                                      row.get(2), row.get(3), row.get(4),
                                      row.get(5), row.get(6), row.get(7)))?;
        macro_rules! set_some {
            ( $object: ident) => {
                macro_rules! set {
                    ($setter: ident, $value: expr ) => {{
                        if let Some(value) = $value {
                            $object.$setter(value);
                        }
                    }}
                }
            }
        }

        set_some!(stats);
        stats.set_created(created);
        set!(set_updated, updated);
        stats.set_encryption_count(encryption_count);
        set!(set_encryption_first, encryption_first);
        set!(set_encryption_last, encryption_last);
        stats.set_verification_count(verification_count);
        set!(set_verification_first, verification_first);
        set!(set_verification_last, verification_last);
        Ok(())
    }
}

/* Iterators.  */

struct StoreIterServer {
    c: Rc<Connection>,
    prefix: String,
    n: ID,
}

impl StoreIterServer {
    fn new(c: Rc<Connection>, prefix: &str) -> Self {
        StoreIterServer{c: c, prefix: String::from(prefix) + "%", n: ID::null()}
    }
}

impl node::store_iter::Server for StoreIterServer {
    fn next(&mut self,
            _: node::store_iter::NextParams,
            mut results: node::store_iter::NextResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let (id, realm, name, network_policy): (ID, String, String, i64) =
            sry!(self.c.query_row(
                 "SELECT id, realm, name, network_policy FROM stores
                      WHERE id > ?1 AND realm like ?2
                      ORDER BY id LIMIT 1",
                &[&self.n, &self.prefix],
                |row| (row.get(0), row.get(1), row.get(2), row.get(3))));

        // We cannot implement FromSql and friends for
        // core::NetworkPolicy, hence we need to do it by foot.
        if network_policy < 0 || network_policy > 3 {
            fail!(node::Error::SystemError);
        }
        let network_policy = core::NetworkPolicy::from(network_policy as u8);

        let mut entry = pry!(results.get().get_result()).init_ok();
        entry.set_realm(&realm);
        entry.set_name(&name);
        entry.set_network_policy(network_policy.into());
        entry.set_store(node::store::ToClient::new(
            StoreServer::new(self.c.clone(), id)).into_client::<capnp_rpc::Server>());
        self.n = id;
        Promise::ok(())
    }
}

struct BindingIterServer {
    c: Rc<Connection>,
    store_id: ID,
    n: ID,
}

impl BindingIterServer {
    fn new(c: Rc<Connection>, store_id: ID) -> Self {
        BindingIterServer{c: c, store_id: store_id, n: ID::null()}
    }
}

impl node::binding_iter::Server for BindingIterServer {
    fn next(&mut self,
            _: node::binding_iter::NextParams,
            mut results: node::binding_iter::NextResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let (id, label, fingerprint): (ID, String, String) =
            sry!(self.c.query_row(
                 "SELECT bindings.id, bindings.label, keys.fingerprint FROM bindings
                      JOIN keys ON bindings.key = keys.id
                      WHERE bindings.id > ?1 AND bindings.store = ?2
                      ORDER BY bindings.id LIMIT 1",
                &[&self.n, &self.store_id],
                |row| (row.get(0), row.get(1), row.get(2))));

        let mut entry = pry!(results.get().get_result()).init_ok();
        entry.set_label(&label);
        entry.set_fingerprint(&fingerprint);
        entry.set_binding(node::binding::ToClient::new(
            BindingServer::new(self.c.clone(), id)).into_client::<capnp_rpc::Server>());
        self.n = id;
        Promise::ok(())
    }
}

struct KeyIterServer {
    c: Rc<Connection>,
    n: ID,
}

impl KeyIterServer {
    fn new(c: Rc<Connection>) -> Self {
        KeyIterServer{c: c, n: ID::null()}
    }
}

impl node::key_iter::Server for KeyIterServer {
    fn next(&mut self,
            _: node::key_iter::NextParams,
            mut results: node::key_iter::NextResults)
            -> Promise<(), capnp::Error> {
        bind_results!(results);
        let (id, fingerprint): (ID, String) =
            sry!(self.c.query_row(
                 "SELECT id, fingerprint FROM keys
                      WHERE keys.id > ?1
                      ORDER BY id LIMIT 1",
                &[&self.n],
                |row| (row.get(0), row.get(1))));

        let mut entry = pry!(results.get().get_result()).init_ok();
        entry.set_fingerprint(&fingerprint);
        entry.set_key(node::key::ToClient::new(
            KeyServer::new(self.c.clone(), id)).into_client::<capnp_rpc::Server>());
        self.n = id;
        Promise::ok(())
    }
}

/* Error handling.  */

impl fmt::Debug for node::Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "node::Error::{}",
               match self {
                   &node::Error::Unspecified => "Unspecified",
                   &node::Error::NotFound => "NotFound",
                   &node::Error::Conflict => "Conflict",
                   &node::Error::SystemError => "SystemError",
                   &node::Error::MalformedTPK => "MalformedTPK",
                   &node::Error::MalformedFingerprint => "MalformedFingerprint",
                   &node::Error::NetworkPolicyViolationOffline =>
                       "NetworkPolicyViolation(Offline)",
                   &node::Error::NetworkPolicyViolationAnonymized =>
                       "NetworkPolicyViolation(Anonymized)",
                   &node::Error::NetworkPolicyViolationEncrypted =>
                       "NetworkPolicyViolation(Encrypted)",
                   &node::Error::NetworkPolicyViolationInsecure =>
                       "NetworkPolicyViolation(Insecure)",
               })
    }
}

impl From<rusqlite::Error> for node::Error {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(f, _) => match f.code {
                rusqlite::ErrorCode::ConstraintViolation =>
                    node::Error::NotFound,
                _ => node::Error::SystemError,
            },
            rusqlite::Error::QueryReturnedNoRows =>
                node::Error::NotFound,
            _ => node::Error::SystemError,
        }
    }
}

impl From<failure::Error> for node::Error {
    fn from(e: failure::Error) -> Self {
        if let Some(e) = e.downcast_ref::<openpgp::Error>() {
            return match e {
                &openpgp::Error::MalformedTPK(_) =>
                    node::Error::MalformedTPK,
                _ => node::Error::SystemError,
            }
        }

        if let Some(e) = e.downcast_ref::<super::Error>() {
            return match e {
                &super::Error::NotFound => node::Error::NotFound,
                &super::Error::Conflict => node::Error::Conflict,
                _ => unreachable!(),
            }
        }

        if let Some(e) = e.downcast_ref::<core::Error>() {
            return match e {
                &core::Error::NetworkPolicyViolation(p) =>
                    match p {
                        core::NetworkPolicy::Offline =>
                            node::Error::NetworkPolicyViolationOffline,
                        core::NetworkPolicy::Anonymized =>
                            node::Error::NetworkPolicyViolationAnonymized,
                        core::NetworkPolicy::Encrypted =>
                            node::Error::NetworkPolicyViolationEncrypted,
                        core::NetworkPolicy::Insecure =>
                            node::Error::NetworkPolicyViolationInsecure,
                    },
                _ => unreachable!(),
            }
        }

        if let Some(e) = e.downcast_ref::<rusqlite::Error>() {
            return match e {
                &rusqlite::Error::SqliteFailure(f, _) => match f.code {
                    rusqlite::ErrorCode::ConstraintViolation =>
                        node::Error::NotFound,
                    _ => node::Error::SystemError,
                },
                &rusqlite::Error::QueryReturnedNoRows =>
                    node::Error::NotFound,
                _ => node::Error::SystemError,
            }
        }

        eprintln!("Error not converted: {:?}", e);
        node::Error::SystemError
    }
}

impl From<openpgp::Error> for node::Error {
    fn from(e: openpgp::Error) -> Self {
        match e {
            openpgp::Error::MalformedTPK(_) =>
                node::Error::MalformedTPK,
            _ => node::Error::SystemError,
        }
    }
}

impl From<core::Error> for node::Error {
    fn from(_: core::Error) -> Self {
        node::Error::SystemError
    }
}

impl From<net::Error> for node::Error {
    fn from(_: net::Error) -> Self {
        node::Error::SystemError
    }
}

impl From<io::Error> for node::Error {
    fn from(_: io::Error) -> Self {
        node::Error::SystemError
    }
}


/* Database schemata and migrations.  */

/* Version 1.  */
const DB_SCHEMA_1: &'static str = "
CREATE TABLE version (
    id INTEGER PRIMARY KEY,
    version INTEGER);

INSERT INTO version (id, version) VALUES (1, 1);

CREATE TABLE stores (
    id INTEGER PRIMARY KEY,
    realm TEXT NOT NULL,
    network_policy INTEGER NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (realm, name));

CREATE TABLE bindings (
    id INTEGER PRIMARY KEY,
    store INTEGER NOT NULL,
    label TEXT NOT NULL,
    key INTEGER NOT NULL,

    created INTEGER NOT NULL,
    updated INTEGER NULL,

    encryption_count DEFAULT 0,
    encryption_first INTEGER NULL,
    encryption_last INTEGER NULL,
    verification_count DEFAULT 0,
    verification_first INTEGER NULL,
    verification_last INTEGER NULL,

    UNIQUE(store, label),
    FOREIGN KEY (store) REFERENCES stores(id) ON DELETE CASCADE,
    FOREIGN KEY (key) REFERENCES keys(id) ON DELETE CASCADE);

CREATE TABLE keys (
    id INTEGER PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    key BLOB,

    created INTEGER NOT NULL,
    updated INTEGER NULL,
    update_at INTEGER NOT NULL,

    encryption_count DEFAULT 0,
    encryption_first INTEGER NULL,
    encryption_last INTEGER NULL,
    verification_count DEFAULT 0,
    verification_first INTEGER NULL,
    verification_last INTEGER NULL,

    UNIQUE (fingerprint));

CREATE TABLE key_by_keyid (
    id INTEGER PRIMARY KEY,
    keyid INTEGER NOT NULL,
    key INTEGER NOT NULL,

    UNIQUE(keyid, key),
    FOREIGN KEY (key) REFERENCES keys(id) ON DELETE CASCADE);

CREATE TABLE log (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,
    level INTEGER NOT NULL,
    store INTEGER NULL,
    binding INTEGER NULL,
    key INTEGER NULL,
    slug TEXT NOT NULL,
    message TEXT NOT NULL,
    error TEXT NULL,
    FOREIGN KEY (store) REFERENCES stores(id) ON DELETE CASCADE,
    FOREIGN KEY (binding) REFERENCES bindings(id) ON DELETE CASCADE,
    FOREIGN KEY (key) REFERENCES keys(id) ON DELETE CASCADE);
";

/* Miscellaneous.  */

impl<'a> From<&'a core::NetworkPolicy> for node::NetworkPolicy {
    fn from(policy: &core::NetworkPolicy) -> Self {
        match policy {
            &core::NetworkPolicy::Offline    => node::NetworkPolicy::Offline,
            &core::NetworkPolicy::Anonymized => node::NetworkPolicy::Anonymized,
            &core::NetworkPolicy::Encrypted  => node::NetworkPolicy::Encrypted,
            &core::NetworkPolicy::Insecure   => node::NetworkPolicy::Insecure,
        }
    }
}

impl From<core::NetworkPolicy> for node::NetworkPolicy {
    fn from(policy: core::NetworkPolicy) -> Self {
        (&policy).into()
    }
}

impl From<node::NetworkPolicy> for core::NetworkPolicy {
    fn from(policy: node::NetworkPolicy) -> Self {
        match policy {
            node::NetworkPolicy::Offline    => core::NetworkPolicy::Offline,
            node::NetworkPolicy::Anonymized => core::NetworkPolicy::Anonymized,
            node::NetworkPolicy::Encrypted  => core::NetworkPolicy::Encrypted,
            node::NetworkPolicy::Insecure   => core::NetworkPolicy::Insecure,
        }
    }
}
