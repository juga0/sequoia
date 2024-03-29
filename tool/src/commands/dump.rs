use std::io::{self, Read};
use time;

extern crate termsize;

extern crate sequoia_openpgp as openpgp;
use openpgp::constants::SymmetricAlgorithm;
use openpgp::conversions::hex;
use openpgp::{Packet, Result};
use openpgp::packet::ctb::CTB;
use openpgp::packet::{Header, BodyLength, Signature};
use openpgp::packet::signature::subpacket::{Subpacket, SubpacketValue};
use openpgp::crypto::{SessionKey, s2k::S2K};
use openpgp::parse::{map::Map, Parse, PacketParserResult};

use super::TIMEFMT;

pub fn dump(input: &mut io::Read, output: &mut io::Write, mpis: bool, hex: bool,
            sk: Option<&SessionKey>)
        -> Result<()> {
    let mut ppr
        = openpgp::parse::PacketParserBuilder::from_reader(input)?
        .map(hex).finalize()?;
    let width = termsize::get().map(|s| s.cols as usize).unwrap_or(80);
    let mut dumper = PacketDumper::new(width, mpis);

    while let PacketParserResult::Some(mut pp) = ppr {
        let additional_fields = match pp.packet {
            Packet::Literal(_) => {
                let mut prefix = vec![0; 40];
                let n = pp.read(&mut prefix)?;
                Some(vec![
                    format!("Content: {:?}{}",
                            String::from_utf8_lossy(&prefix[..n]),
                            if n == prefix.len() { "..." } else { "" }),
                ])
            },
            Packet::SEIP(_) if sk.is_some() => {
                let sk = sk.as_ref().unwrap();
                let mut decrypted_with = None;
                for algo in 1..20 {
                    let algo = SymmetricAlgorithm::from(algo);
                    if let Ok(size) = algo.key_size() {
                        if size != sk.len() { continue; }
                    } else {
                        continue;
                    }

                    if let Ok(_) = pp.decrypt(algo, sk) {
                        decrypted_with = Some(algo);
                        break;
                    }
                }
                let mut fields = Vec::new();
                fields.push(format!("Session key: {}", hex::encode(sk)));
                if let Some(algo) = decrypted_with {
                    fields.push(format!("Symmetric algo: {}", algo));
                    fields.push("Decryption successful".into());
                } else {
                    fields.push("Decryption failed".into());
                }
                Some(fields)
            },
            Packet::AED(_) if sk.is_some() => {
                let sk = sk.as_ref().unwrap();
                let algo = if let Packet::AED(ref aed) = pp.packet {
                    aed.symmetric_algo()
                } else {
                    unreachable!()
                };

                let _ = pp.decrypt(algo, sk);

                let mut fields = Vec::new();
                fields.push(format!("Session key: {}", hex::encode(sk)));
                if pp.decrypted() {
                    fields.push("Decryption successful".into());
                } else {
                    fields.push("Decryption failed".into());
                }
                Some(fields)
            },
            _ => None,
        };

        let header = pp.header().clone();
        let map = pp.take_map();

        let (packet, ppr_) = pp.recurse()?;
        ppr = ppr_;
        let recursion_depth = ppr.last_recursion_depth().unwrap();

        dumper.packet(output, recursion_depth as usize,
                      header, packet, map, additional_fields)?;
    }

    dumper.flush(output)
}

struct Node {
    header: Header,
    packet: Packet,
    map: Option<Map>,
    additional_fields: Option<Vec<String>>,
    children: Vec<Node>,
}

impl Node {
    fn new(header: Header, packet: Packet, map: Option<Map>,
           additional_fields: Option<Vec<String>>) -> Self {
        Node {
            header: header,
            packet: packet,
            map: map,
            additional_fields: additional_fields,
            children: Vec::new(),
        }
    }

    fn append(&mut self, depth: usize, node: Node) {
        if depth == 0 {
            self.children.push(node);
        } else {
            self.children.iter_mut().last().unwrap().append(depth - 1, node);
        }
    }
}

pub struct PacketDumper {
    width: usize,
    mpis: bool,
    root: Option<Node>,
}

impl PacketDumper {
    pub fn new(width: usize, mpis: bool) -> Self {
        PacketDumper {
            width: width,
            mpis: mpis,
            root: None,
        }
    }

    pub fn packet(&mut self, output: &mut io::Write, depth: usize,
                  header: Header, p: Packet, map: Option<Map>,
                  additional_fields: Option<Vec<String>>)
                  -> Result<()> {
        let node = Node::new(header, p, map, additional_fields);
        if self.root.is_none() {
            assert_eq!(depth, 0);
            self.root = Some(node);
        } else {
            if depth == 0 {
                let root = self.root.take().unwrap();
                self.dump_tree(output, "", &root)?;
                self.root = Some(node);
            } else {
                self.root.as_mut().unwrap().append(depth - 1, node);
            }
        }
        Ok(())
    }

    pub fn flush(&self, output: &mut io::Write) -> Result<()> {
        if let Some(root) = self.root.as_ref() {
            self.dump_tree(output, "", &root)?;
        }
        Ok(())
    }

    fn dump_tree(&self, output: &mut io::Write, indent: &str, node: &Node)
                 -> Result<()> {
        let indent_node =
            format!("{}{} ", indent,
                    if node.children.is_empty() { " " } else { "│" });
        self.dump_packet(output, &indent_node, Some(&node.header), &node.packet,
                         node.map.as_ref(), node.additional_fields.as_ref())?;
        if node.children.is_empty() {
            return Ok(());
        }

        let last = node.children.len() - 1;
        for (i, child) in node.children.iter().enumerate() {
            let is_last = i == last;
            write!(output, "{}{}── ", indent,
                   if is_last { "└" } else { "├" })?;
            let indent_child =
                format!("{}{}   ", indent,
                        if is_last { " " } else { "│" });
            self.dump_tree(output, &indent_child, child)?;
        }
        Ok(())
    }

    fn dump_packet(&self, output: &mut io::Write, i: &str,
                  header: Option<&Header>, p: &Packet, map: Option<&Map>,
                  additional_fields: Option<&Vec<String>>)
                  -> Result<()> {
        use self::openpgp::Packet::*;

        if let Some(h) = header {
            write!(output, "{} CTB, {}: ",
                   if let CTB::Old(_) = h.ctb { "Old" } else { "New" },
                   match h.length {
                       BodyLength::Full(n) =>
                           format!("{} bytes", n),
                       BodyLength::Partial(n) =>
                           format!("partial length, {} bytes in first chunk", n),
                       BodyLength::Indeterminate =>
                           "indeterminate length".into(),
                   })?;
        }

        match p {
            Unknown(ref u) => {
                writeln!(output, "Unknown Packet")?;
                writeln!(output, "{}  Tag: {}", i, u.tag())?;
                writeln!(output, "{}  Error: {}", i, u.error())?;
            },

            Signature(ref s) => {
                writeln!(output, "Signature Packet")?;
                writeln!(output, "{}  Version: {}", i, s.version())?;
                writeln!(output, "{}  Type: {}", i, s.sigtype())?;
                writeln!(output, "{}  Pk algo: {}", i, s.pk_algo())?;
                writeln!(output, "{}  Hash algo: {}", i, s.hash_algo())?;
                if s.hashed_area().iter().count() > 0 {
                    writeln!(output, "{}  Hashed area:", i)?;
                    for (_, _, pkt) in s.hashed_area().iter() {
                        self.dump_subpacket(output, i, pkt, s)?;
                    }
                }
                if s.unhashed_area().iter().count() > 0 {
                    writeln!(output, "{}  Unhashed area:", i)?;
                    for (_, _, pkt) in s.unhashed_area().iter() {
                        self.dump_subpacket(output, i, pkt, s)?;
                    }
                }
                writeln!(output, "{}  Hash prefix: {}", i,
                         hex::encode(s.hash_prefix()))?;
                write!(output, "{}  Level: {} ", i, s.level())?;
                match s.level() {
                    0 => writeln!(output, "(signature over data)")?,
                    1 => writeln!(output, "(notarization over signatures \
                                           level 0 and data)")?,
                    n => writeln!(output, "(notarization over signatures \
                                           level <= {} and data)", n - 1)?,
                }
                if self.mpis {
                    use openpgp::crypto::mpis::Signature::*;
                    writeln!(output, "{}", i)?;
                    writeln!(output, "{}  Signature:", i)?;

                    let ii = format!("{}    ", i);
                    match s.mpis() {
                        RSA { s } =>
                            self.dump_mpis(output, &ii,
                                           &[&s.value],
                                           &["s"])?,
                        DSA { r, s } =>
                            self.dump_mpis(output, &ii,
                                           &[&r.value, &s.value],
                                           &["r", "s"])?,
                        Elgamal { r, s } =>
                            self.dump_mpis(output, &ii,
                                           &[&r.value, &s.value],
                                           &["r", "s"])?,
                        EdDSA { r, s } =>
                            self.dump_mpis(output, &ii,
                                           &[&r.value, &s.value],
                                           &["r", "s"])?,
                        ECDSA { r, s } =>
                            self.dump_mpis(output, &ii,
                                           &[&r.value, &s.value],
                                           &["r", "s"])?,
                        Unknown { mpis, rest } => {
                            let keys: Vec<String> =
                                (0..mpis.len()).map(
                                    |i| format!("mpi{}", i)).collect();
                            self.dump_mpis(
                                output, &ii,
                                &mpis.iter().map(|m| m.value.iter().as_slice())
                                    .collect::<Vec<_>>()[..],
                                &keys.iter().map(|k| k.as_str())
                                    .collect::<Vec<_>>()[..],
                            )?;

                            self.dump_mpis(output, &ii, &[&rest[..]], &["rest"])?;
                        },
                    }
                }
            },

            OnePassSig(ref o) => {
                writeln!(output, "One-Pass Signature Packet")?;
                writeln!(output, "{}  Version: {}", i, o.version())?;
                writeln!(output, "{}  Type: {}", i, o.sigtype())?;
                writeln!(output, "{}  Pk algo: {}", i, o.pk_algo())?;
                writeln!(output, "{}  Hash algo: {}", i, o.hash_algo())?;
                writeln!(output, "{}  Issuer: {}", i, o.issuer())?;
                writeln!(output, "{}  Last: {}", i, o.last())?;
            },

            PublicKey(ref k) | PublicSubkey(ref k)
                | SecretKey(ref k) | SecretSubkey(ref k) =>
            {
                writeln!(output, "{}", p.tag())?;
                writeln!(output, "{}  Version: {}", i, k.version())?;
                writeln!(output, "{}  Creation time: {}", i,
                         time::strftime(TIMEFMT, k.creation_time()).unwrap())?;
                writeln!(output, "{}  Pk algo: {}", i, k.pk_algo())?;
                if let Some(bits) = k.mpis().bits() {
                    writeln!(output, "{}  Pk size: {} bits", i, bits)?;
                }
                if self.mpis {
                    use openpgp::crypto::mpis::PublicKey::*;
                    writeln!(output, "{}", i)?;
                    writeln!(output, "{}  Public Key:", i)?;

                    let ii = format!("{}    ", i);
                    match k.mpis() {
                        RSA { e, n } =>
                            self.dump_mpis(output, &ii,
                                           &[&e.value, &n.value],
                                           &["e", "n"])?,
                        DSA { p, q, g, y } =>
                            self.dump_mpis(output, &ii,
                                           &[&p.value, &q.value, &g.value,
                                             &y.value],
                                           &["p", "q", "g", "y"])?,
                        Elgamal { p, g, y } =>
                            self.dump_mpis(output, &ii,
                                           &[&p.value, &g.value, &y.value],
                                           &["p", "g", "y"])?,
                        EdDSA { curve, q } => {
                            writeln!(output, "{}  Curve: {}", ii, curve)?;
                            self.dump_mpis(output, &ii, &[&q.value], &["q"])?;
                        },
                        ECDSA { curve, q } => {
                            writeln!(output, "{}  Curve: {}", ii, curve)?;
                            self.dump_mpis(output, &ii, &[&q.value], &["q"])?;
                        },
                        ECDH { curve, q, hash, sym } => {
                            writeln!(output, "{}  Curve: {}", ii, curve)?;
                            writeln!(output, "{}  Hash algo: {}", ii, hash)?;
                            writeln!(output, "{}  Symmetric algo: {}", ii,
                                     sym)?;
                            self.dump_mpis(output, &ii, &[&q.value], &["q"])?;
                        },
                        Unknown { mpis, rest } => {
                            let keys: Vec<String> =
                                (0..mpis.len()).map(
                                    |i| format!("mpi{}", i)).collect();
                            self.dump_mpis(
                                output, &ii,
                                &mpis.iter().map(|m| m.value.iter().as_slice())
                                    .collect::<Vec<_>>()[..],
                                &keys.iter().map(|k| k.as_str())
                                    .collect::<Vec<_>>()[..],
                            )?;

                            self.dump_mpis(output, &ii, &[&rest[..]], &["rest"])?;
                        },
                    }

                    if let Some(secrets) = k.secret() {
                        use openpgp::crypto::mpis::SecretKey::*;
                        writeln!(output, "{}", i)?;
                        writeln!(output, "{}  Secret Key:", i)?;

                        let ii = format!("{}    ", i);
                        match secrets {
                            openpgp::packet::key::SecretKey::Unencrypted {
                                mpis,
                            } => match mpis {
                                RSA { d, p, q, u } =>
                                    self.dump_mpis(output, &ii,
                                                   &[&d.value, &p.value, &q.value,
                                                     &u.value],
                                                   &["d", "p", "q", "u"])?,
                                DSA { x } =>
                                    self.dump_mpis(output, &ii, &[&x.value],
                                                   &["x"])?,
                                Elgamal { x } =>
                                    self.dump_mpis(output, &ii, &[&x.value],
                                                   &["x"])?,
                                EdDSA { scalar } =>
                                    self.dump_mpis(output, &ii, &[&scalar.value],
                                                   &["scalar"])?,
                                ECDSA { scalar } =>
                                    self.dump_mpis(output, &ii, &[&scalar.value],
                                                   &["scalar"])?,
                                ECDH { scalar } =>
                                    self.dump_mpis(output, &ii, &[&scalar.value],
                                                   &["scalar"])?,
                                Unknown { mpis, rest } => {
                                    let keys: Vec<String> =
                                        (0..mpis.len()).map(
                                            |i| format!("mpi{}", i)).collect();
                                    self.dump_mpis(
                                        output, &ii,
                                        &mpis.iter()
                                            .map(|m| m.value.iter().as_slice())
                                            .collect::<Vec<_>>()[..],
                                        &keys.iter().map(|k| k.as_str())
                                            .collect::<Vec<_>>()[..],
                                    )?;

                                    self.dump_mpis(output, &ii, &[rest],
                                                   &["rest"])?;
                                },
                            },
                            openpgp::packet::key::SecretKey::Encrypted {
                                s2k, algorithm, ciphertext,
                            } => {
                                writeln!(output, "{}", i)?;
                                write!(output, "{}  S2K: ", ii)?;
                                self.dump_s2k(output, &ii, s2k)?;
                                writeln!(output, "{}  Sym. algo: {}", ii,
                                         algorithm)?;
                                self.dump_mpis(output, &ii, &[&ciphertext[..]],
                                               &["ciphertext"])?;
                            },
                        }
                    }
                }
            },

            Trust(ref p) => {
                writeln!(output, "Trust Packet")?;
                writeln!(output, "{}  Value: {}", i, hex::encode(p.value()))?;
            },

            UserID(ref u) => {
                writeln!(output, "User ID Packet")?;
                writeln!(output, "{}  Value: {}", i,
                         String::from_utf8_lossy(u.value()))?;
            },

            UserAttribute(ref u) => {
                use openpgp::packet::user_attribute::{Subpacket, Image};
                writeln!(output, "User Attribute Packet")?;

                for subpacket in u.subpackets() {
                    match subpacket {
                        Ok(Subpacket::Image(image)) => match image {
                            Image::JPEG(data) =>
                                writeln!(output, "{}    JPEG: {} bytes", i,
                                         data.len())?,
                            Image::Private(n, data) =>
                                writeln!(output,
                                         "{}    Private image({}): {} bytes", i,
                                         n, data.len())?,
                            Image::Unknown(n, data) =>
                                writeln!(output,
                                         "{}    Unknown image({}): {} bytes", i,
                                         n, data.len())?,
                        },
                        Ok(Subpacket::Unknown(n, data)) =>
                            writeln!(output,
                                     "{}    Unknown subpacket({}): {} bytes", i,
                                     n, data.len())?,
                        Err(e) =>
                            writeln!(output,
                                     "{}    Invalid subpacket encoding: {}", i,
                                     e)?,
                    }
                }
            },

            Marker(_) => {
                writeln!(output, "Marker Packet")?;
            },

            Literal(ref l) => {
                writeln!(output, "Literal Data Packet")?;
                writeln!(output, "{}  Format: {}", i, l.format())?;
                if let Some(filename) = l.filename() {
                    writeln!(output, "{}  Filename: {}", i,
                             String::from_utf8_lossy(filename))?;
                }
                if let Some(timestamp) = l.date() {
                    writeln!(output, "{}  Timestamp: {}", i,
                             time::strftime(TIMEFMT, timestamp).unwrap())?;
                }
            },

            CompressedData(ref c) => {
                writeln!(output, "Compressed Data Packet")?;
                writeln!(output, "{}  Algorithm: {}", i, c.algorithm())?;
            },

            PKESK(ref p) => {
                writeln!(output, "Public-key Encrypted Session Key Packet")?;
                writeln!(output, "{}  Version: {}", i, p.version())?;
                writeln!(output, "{}  Recipient: {}", i, p.recipient())?;
                writeln!(output, "{}  Pk algo: {}", i, p.pk_algo())?;
                if self.mpis {
                    use openpgp::crypto::mpis::Ciphertext::*;
                    writeln!(output, "{}", i)?;
                    writeln!(output, "{}  Encrypted session key:", i)?;

                    let ii = format!("{}    ", i);
                    match p.esk() {
                        RSA { c } =>
                            self.dump_mpis(output, &ii,
                                           &[&c.value],
                                           &["c"])?,
                        Elgamal { e, c } =>
                            self.dump_mpis(output, &ii,
                                           &[&e.value, &c.value],
                                           &["e", "c"])?,
                        ECDH { e, key } =>
                            self.dump_mpis(output, &ii,
                                           &[&e.value, key],
                                           &["e", "key"])?,
                        Unknown { mpis, rest } => {
                            let keys: Vec<String> =
                                (0..mpis.len()).map(
                                    |i| format!("mpi{}", i)).collect();
                            self.dump_mpis(
                                output, &ii,
                                &mpis.iter().map(|m| m.value.iter().as_slice())
                                    .collect::<Vec<_>>()[..],
                                &keys.iter().map(|k| k.as_str())
                                    .collect::<Vec<_>>()[..],
                            )?;

                            self.dump_mpis(output, &ii, &[rest], &["rest"])?;
                        },
                    }
                }
            },

            SKESK(ref s) => {
                writeln!(output, "Symmetric-key Encrypted Session Key Packet")?;
                writeln!(output, "{}  Version: {}", i, s.version())?;
                match s {
                    openpgp::packet::SKESK::V4(ref s) => {
                        writeln!(output, "{}  Symmetric algo: {}", i,
                                 s.symmetric_algo())?;
                        write!(output, "{}  S2K: ", i)?;
                        self.dump_s2k(output, i, s.s2k())?;
                        if let Some(esk) = s.esk() {
                            writeln!(output, "{}  ESK: {}", i,
                                     hex::encode(esk))?;
                        }
                    },

                    openpgp::packet::SKESK::V5(ref s) => {
                        writeln!(output, "{}  Symmetric algo: {}", i,
                                 s.symmetric_algo())?;
                        writeln!(output, "{}  AEAD: {}", i,
                                 s.aead_algo())?;
                        write!(output, "{}  S2K: ", i)?;
                        self.dump_s2k(output, i, s.s2k())?;
                        writeln!(output, "{}  IV: {}", i,
                                 hex::encode(s.aead_iv()))?;
                        if let Some(esk) = s.esk() {
                            writeln!(output, "{}  ESK: {}", i,
                                     hex::encode(esk))?;
                        }
                        writeln!(output, "{}  Digest: {}", i,
                                 hex::encode(s.aead_digest()))?;
                    },
                }
            },

            SEIP(ref s) => {
                writeln!(output, "Encrypted and Integrity Protected Data Packet")?;
                writeln!(output, "{}  Version: {}", i, s.version())?;
            },

            MDC(ref m) => {
                writeln!(output, "Modification Detection Code Packet")?;
                writeln!(output, "{}  Hash: {}",
                         i, hex::encode(m.hash()))?;
                writeln!(output, "{}  Computed hash: {}",
                         i, hex::encode(m.computed_hash()))?;
            },

            AED(ref a) => {
                writeln!(output, "AEAD Encrypted Data Packet")?;
                writeln!(output, "{}  Version: {}", i, a.version())?;
                writeln!(output, "{}  Symmetric algo: {}", i, a.symmetric_algo())?;
                writeln!(output, "{}  AEAD: {}", i, a.aead())?;
                writeln!(output, "{}  Chunk size: {}", i, a.chunk_size())?;
                writeln!(output, "{}  IV: {}", i, hex::encode(a.iv()))?;
            },
        }

        if let Some(fields) = additional_fields {
            for field in fields {
                writeln!(output, "{}  {}", i, field)?;
            }
        }

        if let Some(map) = map {
            writeln!(output, "{}", i)?;
            let mut hd = hex::Dumper::new(output, self.indentation_for_hexdump(
                i, map.iter().map(|f| f.name.len()).max()
                    .expect("we always have one entry")));

            for field in map.iter() {
                hd.write(field.data, field.name)?;
            }

            let output = hd.into_inner();
            writeln!(output, "{}", i)?;
        } else {
            writeln!(output, "{}", i)?;
        }

        Ok(())
    }

    fn dump_subpacket(&self, output: &mut io::Write, i: &str,
                      s: Subpacket, sig: &Signature)
                      -> Result<()> {
        use self::SubpacketValue::*;

        match s.value {
            Unknown(ref b) =>
                write!(output, "{}    Unknown: {:?}", i, b)?,
            Invalid(ref b) =>
                write!(output, "{}    Invalid: {:?}", i, b)?,
            SignatureCreationTime(ref t) =>
                write!(output, "{}    Signature creation time: {}", i,
                       time::strftime(TIMEFMT, t).unwrap())?,
            SignatureExpirationTime(ref t) =>
                write!(output, "{}    Signature expiration time: {} ({})",
                       i, t,
                       if let Some(creation) = sig.signature_creation_time() {
                           time::strftime(TIMEFMT, &(creation + *t))
                               .unwrap()
                       } else {
                           " (no Signature Creation Time subpacket)".into()
                       })?,
            ExportableCertification(e) =>
                write!(output, "{}    Exportable certification: {}", i, e)?,
            TrustSignature{level, trust} =>
                write!(output, "{}    Trust signature: level {} trust {}", i,
                       level, trust)?,
            RegularExpression(ref r) =>
                write!(output, "{}    Regular expression: {}", i,
                       String::from_utf8_lossy(r))?,
            Revocable(r) =>
                write!(output, "{}    Revocable: {}", i, r)?,
            KeyExpirationTime(ref t) =>
                write!(output, "{}    Key expiration time: {}", i, t)?,
            PreferredSymmetricAlgorithms(ref c) =>
                write!(output, "{}    Symmetric algo preferences: {}", i,
                       c.iter().map(|c| format!("{:?}", c))
                       .collect::<Vec<String>>().join(", "))?,
            RevocationKey{class, pk_algo, ref fp} =>
                write!(output,
                       "{}    Revocation key: class {} algo {} fingerprint {}", i,
                       class, pk_algo, fp)?,
            Issuer(ref is) =>
                write!(output, "{}    Issuer: {}", i, is)?,
            NotationData(ref n) =>
                write!(output, "{}    Notation: {:?}", i, n)?,
            PreferredHashAlgorithms(ref h) =>
                write!(output, "{}    Hash preferences: {}", i,
                       h.iter().map(|h| format!("{:?}", h))
                       .collect::<Vec<String>>().join(", "))?,
            PreferredCompressionAlgorithms(ref c) =>
                write!(output, "{}    Compression preferences: {}", i,
                       c.iter().map(|c| format!("{:?}", c))
                       .collect::<Vec<String>>().join(", "))?,
            KeyServerPreferences(ref p) =>
                write!(output, "{}    Keyserver preferences: {:?}", i, p)?,
            PreferredKeyServer(ref k) =>
                write!(output, "{}    Preferred keyserver: {}", i,
                       String::from_utf8_lossy(k))?,
            PrimaryUserID(p) =>
                write!(output, "{}    Primary User ID: {}", i, p)?,
            PolicyURI(ref p) =>
                write!(output, "{}    Policy URI: {}", i,
                       String::from_utf8_lossy(p))?,
            KeyFlags(ref k) =>
                write!(output, "{}    Key flags: {:?}", i, k)?,
            SignersUserID(ref u) =>
                write!(output, "{}    Signer's User ID: {}", i,
                       String::from_utf8_lossy(u))?,
            ReasonForRevocation{code, ref reason} => {
                let reason = String::from_utf8_lossy(reason);
                write!(output, "{}    Reason for revocation: {}{}{}", i, code,
                       if reason.len() > 0 { ", " } else { "" }, reason)?
            }
            Features(ref f) =>
                write!(output, "{}    Features: {:?}", i, f)?,
            SignatureTarget{pk_algo, hash_algo, ref digest} =>
                write!(output, "{}    Signature target: {}, {}, {}", i,
                       pk_algo, hash_algo, hex::encode(digest))?,
            EmbeddedSignature(_) =>
            // Embedded signature is dumped below.
                write!(output, "{}    Embedded signature: ", i)?,
            IssuerFingerprint(ref fp) =>
                write!(output, "{}    Issuer Fingerprint: {}", i, fp)?,
            PreferredAEADAlgorithms(ref c) =>
                write!(output, "{}    AEAD preferences: {}", i,
                       c.iter().map(|c| format!("{:?}", c))
                       .collect::<Vec<String>>().join(", "))?,
            IntendedRecipient(ref fp) =>
                write!(output, "{}    Intended Recipient: {}", i, fp)?,
        }

        if s.critical {
            write!(output, " (critical)")?;
        }
        writeln!(output)?;

        match s.value {
            EmbeddedSignature(ref sig) => {
                let indent = format!("{}      ", i);
                self.dump_packet(output, &indent, None, sig, None, None)?;
            },
            _ => (),
        }

        Ok(())
    }

    fn dump_s2k(&self, output: &mut io::Write, i: &str, s2k: &S2K)
                -> Result<()> {
        use self::S2K::*;
        match s2k {
            Simple { hash } => {
                writeln!(output, "Simple")?;
                writeln!(output, "{}    Hash: {}", i, hash)?;
            },
            Salted { hash, ref salt } => {
                writeln!(output, "Salted")?;
                writeln!(output, "{}    Hash: {}", i, hash)?;
                writeln!(output, "{}    Salt: {}", i, hex::encode(salt))?;
            },
            Iterated { hash, ref salt, hash_bytes } => {
                writeln!(output, "Iterated")?;
                writeln!(output, "{}    Hash: {}", i, hash)?;
                writeln!(output, "{}    Salt: {}", i, hex::encode(salt))?;
                writeln!(output, "{}    Hash bytes: {}", i, hash_bytes)?;
            },
            Private(n) =>
                writeln!(output, "Private({})", n)?,
            Unknown(n) =>
                writeln!(output, "Unknown({})", n)?,
        }
        Ok(())
    }

    fn dump_mpis(&self, output: &mut io::Write, i: &str,
                 chunks: &[&[u8]], keys: &[&str]) -> Result<()> {
        assert_eq!(chunks.len(), keys.len());
        if chunks.len() == 0 {
            return Ok(());
        }

        let max_key_len = keys.iter().map(|k| k.len()).max().unwrap();

        for (chunk, key) in chunks.iter().zip(keys.iter()) {
            writeln!(output, "{}", i)?;
            let mut hd = hex::Dumper::new(
                Vec::new(), self.indentation_for_hexdump(i, max_key_len));
            hd.write(*chunk, *key)?;
            output.write_all(&hd.into_inner())?;
        }

        Ok(())
    }

    /// Returns indentation for hex dumps.
    ///
    /// Returns a prefix of `i` so that a hexdump with labels no
    /// longer than `max_label_len` will fit into the target width.
    fn indentation_for_hexdump(&self, i: &str, max_label_len: usize) -> String {
        let amount = ::std::cmp::max(
            0,
            ::std::cmp::min(
                self.width as isize
                    - 63 // Length of address, hex digits, and whitespace.
                    - max_label_len as isize,
                i.len() as isize),
        ) as usize;

        format!("{}  ", &i.chars().take(amount).collect::<String>())
    }


}
