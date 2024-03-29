//! Maps various errors to status codes.

use failure;
use std::io;
use libc::c_char;

extern crate sequoia_openpgp as openpgp;

use MoveIntoRaw;
use RefRaw;

/// Complex errors.
///
/// This wraps [`failure::Error`]s.
///
/// [`failure::Error`]: https://docs.rs/failure/0.1.5/failure/struct.Error.html
#[::ffi_wrapper_type(prefix = "pgp_", derive = "Display")]
pub struct Error(failure::Error);

impl<T> From<failure::Fallible<T>> for Status {
    fn from(f: failure::Fallible<T>) -> ::error::Status {
        match f {
            Ok(_) =>  ::error::Status::Success,
            Err(e) => ::error::Status::from(&e),
        }
    }
}

impl ::MoveResultIntoRaw<::error::Status> for ::failure::Fallible<()>
{
    fn move_into_raw(self, errp: Option<&mut *mut ::error::Error>)
                     -> ::error::Status {
        match self {
            Ok(_) => ::error::Status::Success,
            Err(e) => {
                let status = ::error::Status::from(&e);
                if let Some(errp) = errp {
                    *errp = e.move_into_raw();
                }
                status
            },
        }
    }
}

/// Returns the error status code.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "C" fn pgp_error_status(error: *const Error)
                                       -> Status {
    error.ref_raw().into()
}

/// XXX: Reorder and name-space before release.
#[derive(PartialEq, Debug)]
#[repr(C)]
pub enum Status {
    /// The operation was successful.
    Success = 0,

    /// An unknown error occurred.
    UnknownError = -1,

    /// The network policy was violated by the given action.
    NetworkPolicyViolation = -2,

    /// An IO error occurred.
    IoError = -3,

    /// A given argument is invalid.
    InvalidArgument = -15,

    /// The requested operation is invalid.
    InvalidOperation = -4,

    /// The packet is malformed.
    MalformedPacket = -5,

    /// Unsupported packet type.
    UnsupportedPacketType = -14,

    /// Unsupported hash algorithm.
    UnsupportedHashAlgorithm = -9,

    /// Unsupported public key algorithm.
    UnsupportedPublicKeyAlgorithm = -18,

    /// Unsupported elliptic curve.
    UnsupportedEllipticCurve = -21,

    /// Unsupported symmetric algorithm.
    UnsupportedSymmetricAlgorithm = -10,

    /// Unsupported AEAD algorithm.
    UnsupportedAEADAlgorithm = -26,

    /// Unsupported Compression algorithm.
    UnsupportedCompressionAlgorithm = -28,

    /// Unsupport signature type.
    UnsupportedSignatureType = -20,

    /// Invalid password.
    InvalidPassword = -11,

    /// Invalid session key.
    InvalidSessionKey = -12,

    /// Missing session key.
    MissingSessionKey = -27,

    /// Malformed TPK.
    MalformedTPK = -13,

    // XXX: Skipping UnsupportedPacketType = -14.

    // XXX: Skipping InvalidArgument = -15.

    /// Malformed MPI.
    MalformedMPI = -16,

    // XXX: Skipping UnknownPublicKeyAlgorithm = -17.
    // XXX: Skipping UnsupportedPublicKeyAlgorithm = -18

    /// Bad signature.
    BadSignature = -19,

    /// Message has been manipulated.
    ManipulatedMessage = -25,

    // XXX: Skipping UnsupportedSignatureType = -20
    // XXX: Skipping UnsupportedEllipticCurve = -21

    /// Malformed message.
    MalformedMessage = -22,

    /// Index out of range.
    IndexOutOfRange = -23,

    /// TPK not supported.
    UnsupportedTPK = -24,

    // XXX: Skipping ManipulatedMessage = -25
    // XXX: Skipping UnsupportedAEADAlgorithm = -26
    // XXX: Skipping MissingSessionKey = -27
    // XXX: Skipping UnsupportedCompressionAlgorithm = -28
}

/// Returns the error message.
///
/// The returned value must *not* be freed.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "C" fn pgp_status_to_string(status: Status) -> *const c_char {
    use error::Status::*;

    match status {
        Success => "Success\x00",
        UnknownError => "An unknown error occurred\x00",
        NetworkPolicyViolation =>
            "The network policy was violated by the given action\x00",
        IoError => "An IO error occurred\x00",
        InvalidArgument => "A given argument is invalid\x00",
        InvalidOperation => "The requested operation is invalid\x00",
        MalformedPacket => "The packet is malformed\x00",
        UnsupportedPacketType => "Unsupported packet type\x00",
        UnsupportedHashAlgorithm => "Unsupported hash algorithm\x00",
        UnsupportedPublicKeyAlgorithm =>
            "Unsupported public key algorithm\x00",
        UnsupportedEllipticCurve => "Unsupported elliptic curve\x00",
        UnsupportedSymmetricAlgorithm =>
            "Unsupported symmetric algorithm\x00",
        UnsupportedAEADAlgorithm => "Unsupported AEAD algorithm\x00",
        UnsupportedCompressionAlgorithm =>
            "Unsupported compression algorithm\x00",
        UnsupportedSignatureType => "Unsupport signature type\x00",
        InvalidPassword => "Invalid password\x00",
        InvalidSessionKey => "Invalid session key\x00",
        MissingSessionKey => "Missing session key\x00",
        MalformedTPK => "Malformed TPK\x00",
        MalformedMPI => "Malformed MPI\x00",
        BadSignature => "Bad signature\x00",
        ManipulatedMessage => "Message has been manipulated\x00",
        MalformedMessage => "Malformed message\x00",
        IndexOutOfRange => "Index out of range\x00",
        UnsupportedTPK => "TPK not supported\x00",
    }.as_bytes().as_ptr() as *const c_char
}

impl<'a> From<&'a failure::Error> for Status {
    fn from(e: &'a failure::Error) -> Self {
        if let Some(e) = e.downcast_ref::<openpgp::Error>() {
            return match e {
                &openpgp::Error::InvalidArgument(_) =>
                    Status::InvalidArgument,
                &openpgp::Error::InvalidOperation(_) =>
                    Status::InvalidOperation,
                &openpgp::Error::MalformedPacket(_) =>
                    Status::MalformedPacket,
                &openpgp::Error::UnsupportedPacketType(_) =>
                    Status::UnsupportedPacketType,
                &openpgp::Error::UnsupportedHashAlgorithm(_) =>
                    Status::UnsupportedHashAlgorithm,
                &openpgp::Error::UnsupportedPublicKeyAlgorithm(_) =>
                    Status::UnsupportedPublicKeyAlgorithm,
                &openpgp::Error::UnsupportedEllipticCurve(_) =>
                    Status::UnsupportedEllipticCurve,
                &openpgp::Error::UnsupportedSymmetricAlgorithm(_) =>
                    Status::UnsupportedSymmetricAlgorithm,
                &openpgp::Error::UnsupportedAEADAlgorithm(_) =>
                    Status::UnsupportedAEADAlgorithm,
                &openpgp::Error::UnsupportedCompressionAlgorithm(_) =>
                    Status::UnsupportedCompressionAlgorithm,
                &openpgp::Error::UnsupportedSignatureType(_) =>
                    Status::UnsupportedSignatureType,
                &openpgp::Error::InvalidPassword =>
                    Status::InvalidPassword,
                &openpgp::Error::InvalidSessionKey(_) =>
                    Status::InvalidSessionKey,
                &openpgp::Error::MissingSessionKey(_) =>
                    Status::MissingSessionKey,
                &openpgp::Error::MalformedMPI(_) =>
                    Status::MalformedMPI,
                &openpgp::Error::BadSignature(_) =>
                    Status::BadSignature,
                &openpgp::Error::ManipulatedMessage =>
                    Status::ManipulatedMessage,
                &openpgp::Error::MalformedMessage(_) =>
                    Status::MalformedMessage,
                &openpgp::Error::MalformedTPK(_) =>
                    Status::MalformedTPK,
                &openpgp::Error::IndexOutOfRange =>
                    Status::IndexOutOfRange,
                &openpgp::Error::UnsupportedTPK(_) =>
                    Status::UnsupportedTPK,
            }
        }

        if let Some(_) = e.downcast_ref::<io::Error>() {
            return Status::IoError;
        }

        eprintln!("ffi: Error not converted: {}", e);
        Status::UnknownError
    }
}
