//! Helpers the libraries share: file names and OS errors as Lua sees them,
//! temporary files and child processes.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;

use orbit::{Call, Error, Ret};

/// A file name from Lua bytes. On Unix any bytes are a valid name; elsewhere
/// names must be text, and invalid UTF-8 is replaced.
pub(crate) fn os_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(bytes).into()
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).into_owned().into()
    }
}

/// The bytes of an OS string (lossy outside Unix).
#[allow(dead_code, reason = "not every feature set reads OS strings")]
pub(crate) fn os_bytes(s: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// The C library's text for an I/O error (`No such file or directory`),
/// without the ` (os error 2)` that Rust appends. On Windows, where Rust
/// gives the system's own message ("The system cannot find the file
/// specified."), it is the C runtime's text for the error, as Lua reports it.
pub fn os_error_text(e: &io::Error) -> String {
    #[cfg(windows)]
    if e.raw_os_error().is_some() {
        return crate::crt::errno_text(crate::crt::errno(e));
    }
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// The C library's `errno` for an I/O error, the code Lua's file functions
/// return (0 when the error didn't come from the OS).
pub(crate) fn os_error_code(e: &io::Error) -> i64 {
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(_) => i64::from(crate::crt::errno(e)),
        #[cfg(not(windows))]
        Some(code) => i64::from(code),
        None => 0,
    }
}

/// Create a new, empty file with a unique name in the temporary directory
/// (`os.tmpname`, `io.tmpfile`). Creating it, rather than only picking a
/// name, means no other process can take the name first.
#[allow(dead_code, reason = "used by io and os, which are features")]
pub(crate) fn temp_file() -> io::Result<(PathBuf, std::fs::File)> {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let dir = std::env::temp_dir();
    let mut last = None;
    for _ in 0..100 {
        // RandomState is seeded randomly per process; mix in a counter so
        // every attempt differs.
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u32(std::process::id());
        let mut bits = h.finish();
        let mut name = String::from("orbit_");
        for _ in 0..6 {
            name.push(CHARS[(bits % CHARS.len() as u64) as usize] as char);
            bits /= CHARS.len() as u64;
        }
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no unique name")))
}

/// `nil, message, code` for a failed file operation, as Lua's file
/// functions return it. `name`, when given, prefixes the message.
#[cfg(any(feature = "io", feature = "os"))]
pub(crate) fn file_error<'s>(
    c: &'s Call<'s>,
    e: &io::Error,
    name: Option<&[u8]>,
) -> Result<Ret, Error> {
    let (_, msg, code) = file_failure(e, name);
    c.ret((orbit::Value::Nil, &msg[..], code))
}

/// [`file_error`]'s results, for natives on the stack API.
#[cfg(any(feature = "io", feature = "os"))]
pub(crate) type FileFailure = (orbit::stack::Nil, Vec<u8>, i64);

#[cfg(any(feature = "io", feature = "os"))]
pub(crate) fn file_failure(e: &io::Error, name: Option<&[u8]>) -> FileFailure {
    let mut msg = Vec::new();
    if let Some(n) = name {
        msg.extend_from_slice(n);
        msg.extend_from_slice(b": ");
    }
    msg.extend_from_slice(os_error_text(e).as_bytes());
    (orbit::stack::Nil, msg, os_error_code(e))
}

/// The shell command for `cmd`.
#[allow(dead_code, reason = "used by io and os, which are features")]
pub(crate) fn shell(cmd: &OsStr) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// `true|nil, "exit"|"signal", code` for a finished child, as
/// `os.execute` and a popen file's `close` return it.
#[allow(dead_code, reason = "used by io and os, which are features")]
pub(crate) fn exit_status<'s>(
    c: &'s Call<'s>,
    status: std::process::ExitStatus,
) -> Result<Ret, Error> {
    c.ret(exit_results(status))
}

/// What `os.execute` and `close` on a pipe return for a finished command:
/// success (`true` or `nil`), `"exit"` or `"signal"`, and the code.
pub(crate) type ExitResults = (Option<bool>, &'static str, i64);

/// [`exit_status`]'s results, for natives on the stack API.
#[allow(dead_code, reason = "used by io and os, which are features")]
pub(crate) fn exit_results(status: std::process::ExitStatus) -> ExitResults {
    let (what, code): (&'static str, i64) = match status.code() {
        Some(code) => ("exit", i64::from(code)),
        None => {
            #[cfg(unix)]
            let sig = std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0);
            #[cfg(not(unix))]
            let sig = 0;
            ("signal", i64::from(sig))
        }
    };
    let ok = (what == "exit" && code == 0).then_some(true);
    (ok, what, code)
}

/// Flush the output `print` and `io.write` share, so a child process or
/// the exit doesn't overtake it.
#[allow(dead_code, reason = "used by io and os, which are features")]
pub(crate) fn flush_stdout(c: &orbit::Scope<'_>) {
    c.with_stdout(|out| {
        let _ = out.flush();
    });
}

#[cfg(all(test, windows))]
mod tests {
    use super::{os_error_code, os_error_text};
    use std::io::Error;

    #[test]
    fn windows_errors_read_as_the_c_runtime_gives_them() {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND
        for code in [2, 3] {
            let e = Error::from_raw_os_error(code);
            assert_eq!(
                (os_error_code(&e), os_error_text(&e).as_str()),
                (2, "No such file or directory")
            );
        }
        // ERROR_SHARING_VIOLATION
        let e = Error::from_raw_os_error(32);
        assert_eq!(
            (os_error_code(&e), os_error_text(&e).as_str()),
            (13, "Permission denied")
        );
    }
}
