//! The command line as bytes, shared by `orbit` and `orbitc`.
//!
//! `std::env::args()` panics on an argument that is not UTF-8, so the
//! programs read `args_os()`. On Unix an argument is bytes already and is
//! kept unchanged, which is what the reference `lua` puts in `arg`; on
//! Windows arguments are UTF-16 and are converted lossily (an unpaired
//! surrogate becomes U+FFFD). Options are matched on the bytes, and file
//! names go back to paths without passing through `String`.

use std::ffi::OsString;
use std::path::PathBuf;

/// The process's arguments, the program name first.
pub(crate) fn args() -> Vec<Vec<u8>> {
    std::env::args_os().map(bytes).collect()
}

/// An OS string's bytes (lossy outside Unix).
fn bytes(s: OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        s.into_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// The path an argument names.
// `orbit` uses it only for `--debug-connect`
#[cfg_attr(not(feature = "dap"), allow(dead_code))]
pub(crate) fn path(arg: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(arg).into()
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(arg).into_owned().into()
    }
}

/// An argument as text for a message (lossy).
pub(crate) fn text(arg: &[u8]) -> String {
    String::from_utf8_lossy(arg).into_owned()
}
