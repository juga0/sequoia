//! Handles KeyIDs.
//!
//! Wraps [`sequoia-openpgp::KeyID`].
//!
//! [`sequoia-openpgp::KeyID`]: ../../../sequoia_openpgp/enum.KeyID.html

use std::ffi::{CString, CStr};
use std::hash::{Hash, Hasher};
use std::ptr;
use std::slice;
use libc::{uint8_t, uint64_t, c_char};

extern crate sequoia_openpgp;
use self::sequoia_openpgp::KeyID;

use build_hasher;

/// Reads a binary key ID.
#[no_mangle]
pub extern "system" fn sq_keyid_from_bytes(id: *const uint8_t) -> *mut KeyID {
    assert!(!id.is_null());
    let id = unsafe { slice::from_raw_parts(id, 8) };
    Box::into_raw(Box::new(KeyID::from_bytes(id)))
}

/// Reads a hex-encoded Key ID.
#[no_mangle]
pub extern "system" fn sq_keyid_from_hex(id: *const c_char) -> *mut KeyID {
    assert!(!id.is_null());
    let id = unsafe { CStr::from_ptr(id).to_string_lossy() };
    KeyID::from_hex(&id)
        .map(|id| Box::into_raw(Box::new(id)))
        .unwrap_or(ptr::null_mut())
}

/// Frees an `KeyID` object.
#[no_mangle]
pub extern "system" fn sq_keyid_free(keyid: *mut KeyID) {
    if keyid.is_null() { return }
    unsafe {
        drop(Box::from_raw(keyid));
    }
}

/// Clones the KeyID.
#[no_mangle]
pub extern "system" fn sq_keyid_clone(id: Option<&KeyID>)
                                      -> *mut KeyID {
    let id = id.expect("KeyID is NULL");
    box_raw!(id.clone())
}

/// Hashes the KeyID.
#[no_mangle]
pub extern "system" fn sq_keyid_hash(id: Option<&KeyID>)
                                     -> uint64_t {
    let id = id.expect("KeyID is NULL");
    let mut hasher = build_hasher();
    id.hash(&mut hasher);
    hasher.finish()
}

/// Converts the KeyID to its standard representation.
#[no_mangle]
pub extern "system" fn sq_keyid_to_string(id: Option<&KeyID>)
                                          -> *mut c_char {
    let id = id.expect("KeyID is NULL");
    CString::new(id.to_string())
        .unwrap() // Errors only on internal nul bytes.
        .into_raw()
}

/// Converts the KeyID to a hexadecimal number.
#[no_mangle]
pub extern "system" fn sq_keyid_to_hex(id: Option<&KeyID>)
                                       -> *mut c_char {
    let id = id.expect("KeyID is NULL");
    CString::new(id.to_hex())
        .unwrap() // Errors only on internal nul bytes.
        .into_raw()
}

/// Compares KeyIDs.
#[no_mangle]
pub extern "system" fn sq_keyid_equal(a: Option<&KeyID>,
                                      b: Option<&KeyID>)
                                      -> bool {
    let a = a.expect("KeyID 'a' is NULL");
    let b = b.expect("KeyID 'b' is NULL");
    a == b
}