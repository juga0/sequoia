/// A simple signature verification program.
///
/// See https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=872271 for
/// the motivation.

extern crate clap;
extern crate failure;
#[macro_use]
extern crate time;

extern crate openpgp;

use std::process::exit;

use clap::{App, Arg, AppSettings};

use openpgp::{HashAlgo, TPK, Packet, Signature, KeyID};
use openpgp::parse::PacketParser;
use openpgp::tpk::TPKParser;
use openpgp::parse::HashedReader;

// The argument parser.
fn cli_build() -> App<'static, 'static> {
    App::new("sqv")
        .version("0.1.0")
        .about("sqv is a command-line OpenPGP signature verification tool.")
        .setting(AppSettings::ArgRequiredElseHelp)
        .arg(Arg::with_name("keyring").value_name("FILE")
             .help("A keyring")
             .long("keyring")
             .short("r")
             .required(true)
             .takes_value(true)
             .multiple(true))
        .arg(Arg::with_name("signatures").value_name("N")
             .help("The number of valid signatures to return success.  Default: 1")
             .long("signatures")
             .short("n")
             .takes_value(true)
             .multiple(false))
        .arg(Arg::with_name("sig-file").value_name("SIG-FILE")
             .help("File containing the detached signature.")
             .required(true)
             .index(1))
        .arg(Arg::with_name("file").value_name("FILE")
             .help("File to verify.")
             .required(true)
             .index(2))
        .arg(Arg::with_name("trace")
             .help("Trace execution.")
             .long("trace"))
}

fn real_main() -> Result<(), failure::Error> {
    let matches = cli_build().get_matches();

    let trace = matches.is_present("trace");

    let good_threshold
        = if let Some(good_threshold) = matches.value_of("signatures") {
            match good_threshold.parse::<usize>() {
                Ok(good_threshold) => good_threshold,
                Err(err) => {
                    eprintln!("Value passed to --signatures must be numeric: \
                               {} (got: {:?}).",
                              err, good_threshold);
                    exit(2);
                },
            }
        } else {
            1
        };
    if good_threshold < 1 {
        eprintln!("Value passed to --signatures must be >= 1 (got: {:?}).",
                  good_threshold);
        exit(2);
    }


    // First, we collect the signatures and the alleged issuers.
    // Then, we scan the keyrings exactly once to find the associated
    // TPKs.

    // .unwrap() is safe, because "sig-file" is required.
    let sig_file = matches.value_of_os("sig-file").unwrap();

    let mut ppo = PacketParser::from_file(sig_file)?;

    let mut sigs : Vec<(Signature, KeyID, Option<TPK>)> = Vec::new();

    // sig_i is count of all Signature packets that we've seen.  This
    // may be more than sigs.len() if we can't handle some of the
    // sigs.
    let mut sig_i = 0;

    while let Some(pp) = ppo {
        match pp.packet {
            Packet::Signature(ref sig) => {
                sig_i += 1;
                if let Some(fp) = sig.issuer_fingerprint() {
                    if trace {
                        eprintln!("Checking signature allegedly issued by {}.",
                                  fp);
                    }

                    // XXX: We use a KeyID even though we have a
                    // fingerprint!
                    sigs.push((sig.clone(), fp.to_keyid(), None));
                } else if let Some(keyid) = sig.issuer() {
                    if trace {
                        eprintln!("Checking signature allegedly issued by {}.",
                                  keyid);
                    }

                    sigs.push((sig.clone(), keyid, None));
                } else {
                    eprintln!("Signature #{} does not contain information \
                               about the issuer.  Unable to validate.",
                              sig_i);
                }
            },
            Packet::CompressedData(_) => {
                // Skip it.
            },
            ref packet => {
                eprintln!("OpenPGP message is not a detached signature.  \
                           Encountered unexpected packet: {:?} packet.",
                          packet.tag());
                exit(2);
            }
        }

        let (_packet_tmp, _, ppo_tmp, _) = pp.recurse().unwrap();
        ppo = ppo_tmp;
    }

    if sigs.len() == 0 {
        eprintln!("{:?} does not contain an OpenPGP signature.", sig_file);
        exit(2);
    }


    // Hash the content.

    // .unwrap() is safe, because "file" is required.
    let file = matches.value_of_os("file").unwrap();
    let hash_algos : Vec<HashAlgo>
        = sigs.iter().map(|&(ref sig, _, _)| sig.hash_algo).collect();
    let hashes = HashedReader::file(file, &hash_algos[..])?;

    // Find the keys.
    for filename in matches.values_of_os("keyring")
        .expect("No keyring specified.")
    {
        // Iterate over each TPK in the keyring.
        if let Some(pp) = PacketParser::from_file(filename)? {
            for tpk in TPKParser::new(pp.into_iter()) {
                // Iterate over each key in each TPK.
                for key in tpk.keys() {
                    let keyid = key.keyid();

                    // Now, see if we need the key.
                    for &mut (_, ref issuer, ref mut issuer_tpko) in &mut sigs {
                        if *issuer == keyid {
                            if let Some(issuer_tpk) = issuer_tpko.take() {
                                if trace {
                                    eprintln!("Found key {} again.  Merging.",
                                              issuer);
                                }

                                *issuer_tpko
                                    = issuer_tpk.merge(tpk.clone()).ok();
                            } else {
                                if trace {
                                    eprintln!("Found key {}.", issuer);
                                }

                                *issuer_tpko = Some(tpk.clone());
                            }
                        }
                    }
                }
            }
        } else {
            eprintln!("File is empty.");
        }
    }

    // Verify the signatures.
    let mut good = 0;
    for ((mut sig, issuer, tpko), (_hash_algo, mut hash))
        in sigs.into_iter().zip(hashes)
    {
        if trace {
            eprintln!("Checking signature allegedly issued by {}.", issuer);
        }

        if let Some(ref tpk) = tpko {
            // Find the right key.
            for key in tpk.keys() {
                if issuer == key.keyid() {
                    sig.hash(&mut hash);

                    let mut digest = vec![0u8; hash.digest_size()];
                    hash.digest(&mut digest);
                    sig.computed_hash = Some((sig.hash_algo, digest));

                    match sig.verify(key) {
                        Ok(true) => {
                            if trace {
                                eprintln!("Signature by {} is good.", issuer);
                            }
                            println!("{}", tpk.primary().fingerprint());
                            good += 1;
                        },
                        Ok(false) => {
                            if trace {
                                eprintln!("Signature by {} is bad.", issuer);
                            }
                        },
                        Err(err) => {
                            if trace {
                                eprintln!("Verifying signature: {}.", err);
                            }
                        },
                    }

                    break;
                }
            }
        } else {
            eprintln!("Can't verify signature by {}, missing key.",
                      issuer);
        }
    }

    if trace {
        eprintln!("{} of {} signatures are valid (threshold is: {}).",
                  good, sig_i, good_threshold);
    }

    exit(if good >= good_threshold { 0 } else { 1 });
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("{}", e);
        exit(2);
    }
}