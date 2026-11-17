//! I/O errors as the C library reports them, which is what Lua shows: the
//! C library's `errno` and its `strerror` text ("No such file or
//! directory"), on every platform. Rust's own message for an error is not
//! used: its format is not promised, and on Windows it is the system's
//! message ("The system cannot find the file specified.") rather than the
//! C runtime's. `strerror` takes an error number and no script data.
//!
//! It also flushes the C library's own output streams (`fflush(NULL)`)
//! before `io.popen` starts a command, as Lua does: C modules write through
//! those streams, and their buffers are out of the `io` library's reach.
//! It takes no arguments.
//!
//! These are orbit-std's only unsafe calls apart from `ctime.rs` (ADR-6).

#![allow(unsafe_code)]

use std::io;

/// An `errno` carried by an error we build for a failure we detect
/// ourselves, where the system's own error code is not an `errno`
/// (Windows). On Unix such errors are plain OS errors.
#[cfg(windows)]
#[derive(Debug)]
struct Errno(i32);

#[cfg(windows)]
impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&errno_text(self.0))
    }
}

#[cfg(windows)]
impl std::error::Error for Errno {}

/// The error the system gives for `errno`, for a failure we detect before
/// asking the system (a negative seek offset, an operation on a closed
/// file), so it reads exactly as the system's own would.
#[cfg(feature = "io")]
pub(crate) fn system_error(errno: i32) -> io::Error {
    #[cfg(unix)]
    {
        io::Error::from_raw_os_error(errno)
    }
    #[cfg(windows)]
    {
        io::Error::other(Errno(errno))
    }
}

/// The C library's `errno` for an I/O error, if it came from the system.
pub(crate) fn errno(e: &io::Error) -> Option<i32> {
    #[cfg(unix)]
    {
        e.raw_os_error()
    }
    #[cfg(windows)]
    {
        if let Some(Errno(n)) = e.get_ref().and_then(|i| i.downcast_ref::<Errno>()) {
            return Some(*n);
        }
        e.raw_os_error().map(|code| windows_errno(code, e.kind()))
    }
}

/// The C runtime's `errno` for a Win32 error. The C runtime keeps its own
/// Win32-to-`errno` mapping private, so this goes by std's mapping of the
/// error to a kind. Kinds with no C equivalent become `EINVAL`, as
/// unmapped codes do in the C runtime.
#[cfg(windows)]
fn windows_errno(code: i32, kind: io::ErrorKind) -> i32 {
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION (a file another
    // process holds open), which std leaves uncategorized.
    if let 32 | 33 = code {
        return libc::EACCES;
    }
    // ERROR_DIRECTORY (a name ending in `/` that is a file or missing,
    // opened for writing), which std calls NotADirectory but the C
    // runtime reports as EINVAL.
    if code == 267 {
        return libc::EINVAL;
    }
    // The kind by its name: std has mapped Win32 codes to all of these
    // since before the MSRV, but most of the variants can only be named
    // from Rust 1.83 on.
    match format!("{kind:?}").as_str() {
        "NotFound" => libc::ENOENT,
        "ArgumentListTooLong" => libc::E2BIG,
        "WouldBlock" | "ResourceBusy" => libc::EAGAIN,
        "OutOfMemory" => libc::ENOMEM,
        "PermissionDenied" | "ReadOnlyFilesystem" => libc::EACCES,
        "AlreadyExists" => libc::EEXIST,
        "CrossesDevices" => libc::EXDEV,
        "NotADirectory" => libc::ENOTDIR,
        "IsADirectory" => libc::EISDIR,
        "StorageFull" => libc::ENOSPC,
        "BrokenPipe" => libc::EPIPE,
        "InvalidFilename" => libc::ENAMETOOLONG,
        "DirectoryNotEmpty" => libc::ENOTEMPTY,
        _ => libc::EINVAL,
    }
}

/// The C library's text for an `errno`.
#[cfg(unix)]
pub(crate) fn errno_text(errno: i32) -> String {
    // The XSI `strerror_r` (the libc crate links glibc's
    // `__xpg_strerror_r`), which fills our buffer rather than returning a
    // shared one. For an unknown number it fails but still writes the
    // C library's "Unknown error N", which is what `strerror` gives.
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes, and
    // strerror_r writes at most that many, NUL included.
    unsafe { libc::strerror_r(errno, buf.as_mut_ptr().cast(), buf.len()) };
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if len == 0 {
        return format!("Unknown error {errno}");
    }
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// The C runtime's text for an `errno`.
#[cfg(windows)]
pub(crate) fn errno_text(errno: i32) -> String {
    // SAFETY: strerror takes any value and returns a NUL-terminated string
    // in a per-thread buffer, which is copied before anything else on this
    // thread can call it again.
    let text = unsafe { libc::strerror(errno) };
    if text.is_null() {
        return String::new();
    }
    // SAFETY: as above; `text` is non-null and NUL-terminated.
    unsafe { std::ffi::CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// Flush every output stream of the C library (`fflush(NULL)`): buffers
/// that C modules wrote through `FILE*` streams, the C library's `stdout`
/// among them. Failures are ignored, as `io.popen` has nothing to report
/// them through.
#[cfg(feature = "io")]
pub(crate) fn flush_c_streams() {
    // SAFETY: a null stream asks the C library to flush every open output
    // stream; no pointer of ours is passed or kept.
    unsafe { libc::fflush(std::ptr::null_mut()) };
}
