//! IO primitives for Sequoia.

use std::fs::File;
use std::io::{self, Read, Write, Cursor};
use std::path::Path;
use std::slice;
use libc::{uint8_t, c_void, c_char, c_int, size_t, ssize_t, realloc};

#[cfg(unix)]
use std::os::unix::io::FromRawFd;

extern crate sequoia_openpgp as openpgp;

use Maybe;

/// Wraps a generic reader.
#[::ffi_wrapper_type(prefix = "pgp_")]
pub struct Reader(ReaderKind);

/// Specializes readers.
///
/// In some cases, we want to call functions on concrete types.  To
/// avoid nasty hacks, we have specialized variants for that.
pub(crate) enum ReaderKind {
    Generic(Box<io::Read>),
    Armored(openpgp::armor::Reader<'static>),
}

impl Read for ReaderKind {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use self::ReaderKind::*;
        match self {
            Generic(ref mut r) => r.read(buf),
            Armored(ref mut r) => r.read(buf),
        }
    }
}

/// Opens a file returning a reader.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_reader_from_file(errp: Option<&mut *mut ::error::Error>,
                                            filename: *const c_char)
                                            -> Maybe<Reader> {
    let filename = ffi_param_cstr!(filename).to_string_lossy().into_owned();
    File::open(Path::new(&filename))
        .map(|r| ReaderKind::Generic(Box::new(r)))
        .map_err(|e| ::failure::Error::from(e))
        .move_into_raw(errp)
}

/// Opens a file descriptor returning a reader.
#[cfg(unix)]
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_reader_from_fd(fd: c_int)
                                          -> *mut Reader {
    ReaderKind::Generic(Box::new(unsafe {
        File::from_raw_fd(fd)
    })).move_into_raw()
}

/// Creates a reader from a buffer.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_reader_from_bytes(buf: *const uint8_t,
                                             len: size_t)
                                             -> *mut Reader {
    assert!(!buf.is_null());
    let buf = unsafe {
        slice::from_raw_parts(buf, len as usize)
    };
    ReaderKind::Generic(Box::new(Cursor::new(buf))).move_into_raw()
}

/// Reads up to `len` bytes into `buf`.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_reader_read(errp: Option<&mut *mut ::error::Error>,
                                       reader: *mut Reader,
                                       buf: *mut uint8_t, len: size_t)
                                       -> ssize_t {
    assert!(!buf.is_null());
    let buf = unsafe {
        slice::from_raw_parts_mut(buf, len as usize)
    };
    reader.ref_mut_raw().read(buf)
        .map(|n_read| n_read as ssize_t)
        .unwrap_or_else(|e| {
            if let Some(errp) = errp {
                *errp = ::failure::Error::from(e).move_into_raw();
            };

            // Signal failure.
            -1
        })
}


/// Opens a file returning a writer.
///
/// The file will be created if it does not exist, or be truncated
/// otherwise.  If you need more control, use `pgp_writer_from_fd`.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_from_file(errp: Option<&mut *mut ::error::Error>,
                                            filename: *const c_char)
                                            -> *mut Box<Write> {
    ffi_make_fry_from_errp!(errp);
    let filename = ffi_param_cstr!(filename).to_string_lossy().into_owned();
    ffi_try_box!(File::create(Path::new(&filename))
                 .map(|r| Box::new(r))
                 .map_err(|e| ::failure::Error::from(e)))
}

/// Opens a file descriptor returning a writer.
#[cfg(unix)]
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_from_fd(fd: c_int)
                                          -> *mut Box<Write> {
    box_raw!(Box::new(unsafe { File::from_raw_fd(fd) }))
}

/// Creates a writer from a buffer.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_from_bytes(buf: *mut uint8_t,
                                             len: size_t)
                                             -> *mut Box<Write> {
    assert!(!buf.is_null());
    let buf = unsafe {
        slice::from_raw_parts_mut(buf, len as usize)
    };
    box_raw!(Box::new(Cursor::new(buf)))
}

/// Creates an allocating writer.
///
/// This writer allocates memory using `malloc`, and stores the
/// pointer to the memory and the number of bytes written to the given
/// locations `buf`, and `len`.  Both must either be set to zero, or
/// reference a chunk of memory allocated using libc's heap allocator.
/// The caller is responsible to `free` it once the writer has been
/// destroyed.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_alloc(buf: *mut *mut c_void,
                                        len: *mut size_t)
                                        -> *mut Box<Write> {
    let buf = ffi_param_ref_mut!(buf);
    let len = ffi_param_ref_mut!(len);

    box_raw!(Box::new(WriterAlloc {
        buf: buf,
        len: len,
    }))
}

struct WriterAlloc {
    buf: &'static mut *mut c_void,
    len: &'static mut size_t,
}

impl Write for WriterAlloc {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let old_len = *self.len;
        let new_len = old_len + buf.len();

        let new = unsafe {
            realloc(*self.buf, new_len)
        };
        if new.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "out of memory"));
        }

        *self.buf = new;
        *self.len = new_len;

        let sl = unsafe {
            slice::from_raw_parts_mut(new as *mut u8, new_len)
        };
        &mut sl[old_len..].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Do nothing.
        Ok(())
    }
}

/// Frees a writer.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_free(writer: Option<&mut Box<Write>>) {
    ffi_free!(writer)
}

/// Writes up to `len` bytes of `buf` into `writer`.
#[::sequoia_ffi_macros::extern_fn] #[no_mangle]
pub extern "system" fn pgp_writer_write(errp: Option<&mut *mut ::error::Error>,
                                        writer: *mut Box<Write>,
                                        buf: *const uint8_t, len: size_t)
                                        -> ssize_t {
    ffi_make_fry_from_errp!(errp);
    let writer = ffi_param_ref_mut!(writer);
    assert!(!buf.is_null());
    let buf = unsafe {
        slice::from_raw_parts(buf, len as usize)
    };
    ffi_try_or!(writer.write(buf).map_err(|e| ::failure::Error::from(e)), -1) as ssize_t
}
