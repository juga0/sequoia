//! OpenPGP Header.

use {
    Error,
    Result,
    BodyLength,
};
use packet::tag::Tag;
use packet::ctb::CTB;

/// An OpenPGP packet's header.
#[derive(Clone, Debug)]
pub struct Header {
    /// The packet's CTB.
    pub ctb: CTB,
    /// The packet's length.
    pub length: BodyLength,
}

impl Header {
    /// Syntax checks the header.
    ///
    /// A header is consider invalid if:
    ///
    ///   - The tag is Tag::Reserved or Tag::Marker.
    ///   - The tag is unknown (if future_compatible is false).
    ///   - The [length encoding] is invalid for the packet.
    ///   - The lengths are unreasonable for a packet (e.g., a
    ///     >10 kb large PKESK or SKESK).
    ///
    /// [length encoding]: https://tools.ietf.org/html/rfc4880#section-4.2.2.4
    ///
    /// This function does not check the packet's content.  Use
    /// `PacketParser::plausible` for that.
    pub fn valid(&self, future_compatible: bool) -> Result<()> {
        let tag = self.ctb.tag;

        // Reserved packets are Marker packets are never valid.
        if tag == Tag::Reserved || tag == Tag::Marker {
            return Err(Error::UnsupportedPacketType(tag).into());
        }

        // Unknown packets are not valid unless we want future
        // compatibility.
        if ! future_compatible
            && (destructures_to!(Tag::Unknown(_) = tag)
                || destructures_to!(Tag::Private(_) = tag))
        {
            return Err(Error::UnsupportedPacketType(tag).into());
        }

        // An implementation MAY use Partial Body Lengths for data
        // packets, be they literal, compressed, or encrypted.  The
        // first partial length MUST be at least 512 octets long.
        // Partial Body Lengths MUST NOT be used for any other packet
        // types.
        //
        // https://tools.ietf.org/html/rfc4880#section-4.2.2.4
        if tag == Tag::Literal || tag == Tag::CompressedData
            || tag == Tag::SED || tag == Tag::SEIP
            || tag == Tag::AED
        {
            // Data packet.
            if let BodyLength::Partial(l) = self.length {
                if l < 512 {
                    return Err(Error::MalformedPacket(
                        format!("Partial body length must be at least 512 (got: {})",
                            l)).into());
                }
            }
        } else {
            // Non-data packet.
            match self.length {
                BodyLength::Indeterminate =>
                    return Err(Error::MalformedPacket(
                        format!("Indeterminite length encoding not allowed for {} packets",
                                tag)).into()),
                BodyLength::Partial(_) =>
                    return Err(Error::MalformedPacket(
                        format!("Partial Body Chunking not allowed for {} packets",
                                tag)).into()),
                BodyLength::Full(l) => {
                    let valid = match tag {
                        Tag::Signature =>
                            l < (10  /* Header, fixed sized fields.  */
                                 + 2 * 64 * 1024 /* Hashed & Unhashed areas.  */
                                 + 64 * 1024 /* MPIs.  */),
                        Tag::PKESK | Tag::SKESK => l < 10 * 1024,
                        Tag::OnePassSig if ! future_compatible => l == 13,
                        Tag::OnePassSig => l < 1024,
                        Tag::PublicKey | Tag::PublicSubkey
                            | Tag::SecretKey | Tag::SecretSubkey =>
                            l < 1024 * 1024,
                        Tag::Trust => true,
                        Tag::UserID => l < 32 * 1024,
                        Tag::UserAttribute => true,
                        Tag::MDC => l == 20,

                        Tag::Literal | Tag::CompressedData
                            | Tag::SED | Tag::SEIP | Tag::AED
                            | Tag::Unknown(_) | Tag::Private(_) => true,

                        Tag::Reserved | Tag::Marker => true,
                    };

                    if ! valid {
                        return Err(Error::MalformedPacket(
                            format!("Invalid size ({} bytes) for a {} packet",
                                    l, tag)).into())
                    }
                }
            }
        }

        Ok(())
    }
}