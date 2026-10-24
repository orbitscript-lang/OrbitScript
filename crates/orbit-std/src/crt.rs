//! I/O errors as the Windows C runtime reports them, which is what Lua shows
//! there. Rust's `io::Error` gives the Win32 code and the system's message
//! ("The system cannot find the file specified."); Lua gives the C runtime's
//! `errno` and `strerror` text ("No such file or directory"). The C runtime
//! keeps its own Win32-to-`errno` mapping private, so the `errno` comes from
//! std's mapping of the error to a kind; the text comes from `strerror`, the
//! one unsafe call in orbit-std (ADR-6).

#![allow(unsafe_code)]

use std::ffi::CStr;
use std::io::{self, ErrorKind as K};

/// The C runtime's `errno` for an I/O error. Kinds with no C equivalent
/// become `EINVAL`, as unmapped codes do in the C runtime.
pub(crate) fn errno(e: &io::Error) -> i32 {
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION (a file another
    // process holds open), which std leaves uncategorized.
    if let Some(32 | 33) = e.raw_os_error() {
        return libc::EACCES;
    }
    match e.kind() {
        K::NotFound => libc::ENOENT,
        K::ArgumentListTooLong => libc::E2BIG,
        K::WouldBlock | K::ResourceBusy => libc::EAGAIN,
        K::OutOfMemory => libc::ENOMEM,
        K::PermissionDenied | K::ReadOnlyFilesystem => libc::EACCES,
        K::AlreadyExists => libc::EEXIST,
        K::CrossesDevices => libc::EXDEV,
        K::NotADirectory => libc::ENOTDIR,
        K::IsADirectory => libc::EISDIR,
        K::StorageFull => libc::ENOSPC,
        K::BrokenPipe => libc::EPIPE,
        K::InvalidFilename => libc::ENAMETOOLONG,
        K::DirectoryNotEmpty => libc::ENOTEMPTY,
        _ => libc::EINVAL,
    }
}

/// The C runtime's text for an `errno`.
pub(crate) fn errno_text(errno: i32) -> String {
    // SAFETY: strerror takes any value and returns a NUL-terminated string
    // in a per-thread buffer, which is copied before anything else on this
    // thread can call it again.
    let text = unsafe { libc::strerror(errno) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: as above; `text` is non-null and NUL-terminated.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}
