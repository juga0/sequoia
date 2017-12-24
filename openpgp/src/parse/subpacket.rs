//! OpenPGP signature packets include a set of key-value attributes
//! called subpackets.  These subpackets are used to indicate when a
//! signature was created, who created the signature, user &
//! implementation preferences, etc.  The full details are in [Section
//! 5.2.3.1 of RFC 4880].
//!
//! [Section 5.2.3.1 of RFC 4880]: https://tools.ietf.org/html/rfc4880#section-5.2.3.1
//!
//! The standard assigns each subpacket a numeric id, and describes
//! the format of its value.  One subpacket is called Notation Data
//! and is intended as a generic key-value store.  The combined size
//! of the subpackets (including notation data) is limited to 64 KB.
//!
//! Subpackets and notations can be marked as critical.  If an OpenPGP
//! implementation processes a packet that includes critical
//! subpackets or notations that it does not understand, it is
//! required to abort processing.  This allows for forwards compatible
//! changes by indicating whether it is safe to ignore an unknown
//! subpacket or notation.
//!
//! # Examples
//!
//! If a signature packet includes an issuer fingerprint subpacket,
//! print it:
//!
//! ```rust
//! # use openpgp::Packet;
//! # use openpgp::parse::PacketParser;
//! #
//! # f(include_bytes!("../../tests/data/messages/signed.gpg"));
//! #
//! # fn f(message_data: &[u8]) -> Result<(), std::io::Error> {
//! let mut ppo = PacketParser::from_bytes(message_data)?;
//! while let Some(mut pp) = ppo {
//!     if let Packet::Signature(ref sig) = pp.packet {
//!         if let Some((_critical, fp)) = sig.issuer_fingerprint() {
//!             eprintln!("Signature issued by: {}", fp.to_string());
//!         }
//!     }
//!
//!     // Get the next packet.
//!     let (_packet, _packet_depth, tmp, _pp_depth) = pp.recurse()?;
//!     ppo = tmp;
//! }
//! # Ok(())
//! # }
//! ```

use std::io::Error;

use super::*;

#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
fn path_to(artifact: &str) -> PathBuf {
    [env!("CARGO_MANIFEST_DIR"), "tests", "data", "messages", artifact]
        .iter().collect()
}

/// The subpacket types specified by [Section 5.2.3.1 of RFC 4880].
///
/// [Section 5.2.3.1 of RFC 4880]: https://tools.ietf.org/html/rfc4880#section-5.2.3.1
#[derive(Debug)]
#[derive(FromPrimitive)]
#[derive(ToPrimitive)]
#[derive(PartialEq)]
#[derive(Clone, Copy)]
pub enum SubpacketTag {
    Reserved0 = 0,
    Reserved1 = 1,
    SignatureCreationTime = 2,
    SignatureExpirationTime = 3,
    ExportableCertification = 4,
    TrustSignature = 5,
    RegularExpression = 6,
    Revocable = 7,
    Reserved = 8,
    KeyExpirationTime = 9,
    PlaceholderForBackwardCompatibility = 10,
    PreferredSymmetricAlgorithms = 11,
    RevocationKey = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    Reserved15 = 15,
    Issuer = 16,
    Reserved17 = 17,
    Reserved18 = 18,
    Reserved19 = 19,
    NotationData = 20,
    PreferredHashAlgorithms = 21,
    PreferredCompressionAlgorithms = 22,
    KeyServerPreferences = 23,
    PreferredKeyServer = 24,
    PrimaryUserID = 25,
    PolicyURI = 26,
    KeyFlags = 27,
    SignersUserID = 28,
    ReasonForRevocation = 29,
    Features = 30,
    SignatureTarget = 31,
    EmbeddedSignature = 32,
    // Added in RFC 4880bis.
    IssuerFingerprint = 33,
    Private100 = 100,
    Private101 = 101,
    Private102 = 102,
    Private103 = 103,
    Private104 = 104,
    Private105 = 105,
    Private106 = 106,
    Private107 = 107,
    Private108 = 108,
    Private109 = 109,
    Private110 = 110,
}

// Struct holding an arbitrary subpacket.
//
// The value is uninterpreted.  To get a well-structured value, use
// one of `Signature`'s methods, like `Signature::issuer_fingerprint`.
#[derive(Debug,Clone)]
pub struct Subpacket<'a> {
    pub critical: bool,
    pub tag: SubpacketTag,
    pub value: &'a str,
}

/// Decode a subpacket length as described in Section 5.2.3.1 of RFC 4880.
fn subpacket_length(bio: &mut BufferedReaderMemory)
      -> Result<u32, Error> {
    let octet1 = bio.data_consume_hard(1)?[0];
    if octet1 < 192 {
        // One octet.
        return Ok(octet1 as u32);
    }
    if 192 <= octet1 && octet1 < 255 {
        // Two octets length.
        let octet2 = bio.data_consume_hard(1)?[0];
        return Ok(((octet1 as u32 - 192) << 8) + octet2 as u32 + 192);
    }

    // Five octets.
    assert_eq!(octet1, 255);
    return Ok(bio.read_be_u32()?);
}

impl Signature {
    // Initialize `Signature::hashed_area_parsed` from
    // `Signature::hashed_area`, if necessary.
    fn subpackets_init(&self) -> Result<(), Error> {
        if self.hashed_area_parsed.borrow().is_some() {
            return Ok(());
        }

        let mut bio = BufferedReaderMemory::new(&self.hashed_area.as_slice());

        let mut hash = HashMap::new();

        while bio.data(1)?.len() > 0 {
            let len = subpacket_length(&mut bio)?;

            if bio.total_out() + len as usize > self.hashed_area.len() {
                // Subpacket extends beyond the end of the hashed
                // area.  Skip it.
                eprintln!("Invalid subpacket: subpacket extends beyond \
                           end of hashed area ([{}..{}); {}).",
                          bio.total_out(), len, self.hashed_area.len());
                break;
            }

            if len == 0 {
                // Hmm, a zero length packet.  In that case, there is
                // no header.
                continue;
            }

            let tag : u8 = bio.data_consume_hard(1)?[0];
            let len = len - 1;

            // The critical bit is the high bit.  Extract it.
            let critical = tag & (1 << 7) != 0;
            // Then clear it from the type.
            let tag = tag & !(1 << 7);

            let start = bio.total_out();
            assert!(start <= std::u16::MAX as usize);
            assert!(len <= std::u16::MAX as u32);

            hash.insert(tag, (critical, bio.total_out() as u16, len as u16));

            bio.consume(len as usize);
        }

        *self.hashed_area_parsed.borrow_mut() = Some(hash);

        return Ok(());
    }

    /// Returns the specified subpacket.
    ///
    /// This is a generic method; the value is an unstructured byte
    /// stream.  In general, you should prefer to use methods like
    /// `Signature::issuer_fingerprint` to lookup specific subpackets.
    pub fn subpacket<'a>(&'a self, tag: u8) -> Option<(bool, &'a [u8])> {
        let _ = self.subpackets_init();

        match self.hashed_area_parsed.borrow().as_ref().unwrap().get(&tag) {
            Some(&(critical, start, len)) =>
                Some((critical,
                      &self.hashed_area[start as usize
                                        ..start as usize + len as usize])),
            None => None,
        }
    }

    pub fn signature_create_time(&self) {
        let _value = self.subpacket(SubpacketTag::SignatureCreationTime as u8);
        unimplemented!();
    }

    pub fn signature_expiration_time(&self) {
        let _value = self.subpacket(SubpacketTag::SignatureExpirationTime as u8);
        unimplemented!();
    }

    // ExportableCertification
    // TrustSignature
    // RegularExpression
    // Revocable
    // KeyExpirationTime
    // PreferredSymmetricAlgorithms
    // RevocationKey
    // Issuer
    // NotationData
    // PreferredHashAlgorithms
    // PreferredCompressionAlgorithms
    // KeyServerPreferences
    // PreferredKeyServer
    // PrimaryUserID
    // PolicyURI
    // KeyFlags
    // SignersUserID
    // ReasonForRevocation
    // Features
    // SignatureTarget
    // EmbeddedSignature

    /// Return the value of the Issuer Fingerprint subpacket.
    ///
    /// If the subpacket is not present, this returns `None`.
    pub fn issuer_fingerprint(&self) -> Option<(bool, Fingerprint)> {
        match self.subpacket(SubpacketTag::IssuerFingerprint as u8) {
            Some((critical, raw)) => {
                let version = raw.get(0);
                if let Some(version) = version {
                    if *version == 4 {
                        return Some((critical,
                                     Fingerprint::from_bytes(&raw[1..])));
                    }
                }

                // No idea what this is or even if the version is
                // valid.
                return Some((critical, Fingerprint::from_bytes(&raw[..])));
            },
            None => return None,
        }
    }
}

#[test]
fn subpacket_test_1 () {
    let path = path_to("signed.gpg");
    let message = Message::from_file(&path).unwrap();
    eprintln!("Message has {} top-level packets.", message.children().len());
    eprintln!("Message: {:?}", message);

    let mut count = 0;
    for p in message.descendants() {
        if let &Packet::Signature(ref sig) = p {
            count += 1;

            let mut got2 = false;
            let mut got33 = false;

            for i in 0..256 {
                if let Some((critical, _value)) = sig.subpacket(i as u8) {
                    // eprintln!("  {}: {:?}", i, value);

                    if i == 2 {
                        got2 = true;
                        assert!(!critical);
                    } else if i == 33 {
                        got33 = true;
                        assert!(!critical);
                    } else {
                        panic!("Unexpectedly found subpacket {}", i);
                    }
                }
            }

            assert!(got2 && got33);

            let fp = sig.issuer_fingerprint().unwrap().1.to_string();
            // eprintln!("Issuer: {}", fp);
            assert!(
                fp == "7FAF 6ED7 2381 4355 7BDF  7ED2 6863 C9AD 5B4D 22D3"
                || fp == "C03F A641 1B03 AE12 5764  6118 7223 B566 78E0 2528");

            let hex = sig.issuer_fingerprint().unwrap().1.to_hex();
            assert!(
                hex == "7FAF6ED7238143557BDF7ED26863C9AD5B4D22D3"
                || hex == "C03FA6411B03AE12576461187223B56678E02528");
        }
    }
    // 2 packets have subpackets.
    assert_eq!(count, 2);
}
