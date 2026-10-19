//! Local sockets for the debugger (ADR-12, D11): a per-user directory only
//! its owner can enter, and sockets named so a reused pid never lands on
//! an old name. No TCP. On Windows the sockets come from [`crate::win`]
//! (D12).

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(unix)]
type RawStream = std::os::unix::net::UnixStream;
#[cfg(unix)]
type RawListener = std::os::unix::net::UnixListener;
#[cfg(windows)]
type RawStream = crate::win::Socket;
#[cfg(windows)]
type RawListener = crate::win::Listener;

/// A connected local socket.
#[derive(Debug)]
pub(crate) struct Stream(RawStream);

impl Stream {
    pub(crate) fn connect(path: &Path) -> io::Result<Stream> {
        RawStream::connect(path).map(Stream)
    }

    pub(crate) fn try_clone(&self) -> io::Result<Stream> {
        self.0.try_clone().map(Stream)
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }

    pub(crate) fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A listening local socket.
#[derive(Debug)]
pub(crate) struct Listener(RawListener);

impl Listener {
    pub(crate) fn accept(&self) -> io::Result<Stream> {
        #[cfg(unix)]
        let s = self.0.accept().map(|(s, _)| s);
        #[cfg(windows)]
        let s = self.0.accept();
        s.map(Stream)
    }

    pub(crate) fn try_clone(&self) -> io::Result<Listener> {
        self.0.try_clone().map(Listener)
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// The directory the sockets go in: `$XDG_RUNTIME_DIR/orbit`, else
/// `$TMPDIR/orbit` on macOS (already per user), else
/// `$TMPDIR/orbit-<uid>` or `/tmp/orbit-<uid>`. It is created mode 0700
/// and refused if someone else owns it or can enter it.
#[cfg(unix)]
pub(crate) fn dir() -> io::Result<PathBuf> {
    let uid = my_uid()?;
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("orbit"),
        None if cfg!(target_os = "macos") => std::env::temp_dir().join("orbit"),
        None => std::env::temp_dir().join(format!("orbit-{uid}")),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = std::fs::symlink_metadata(&dir)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} must be a directory only its owner can use",
            dir.display()
        )));
    }
    Ok(dir)
}

/// The directory the sockets go in: `%LOCALAPPDATA%\orbit`, which only its
/// user can reach.
#[cfg(windows)]
pub(crate) fn dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    let dir = PathBuf::from(base).join("orbit");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// This process's user id, from a file it owns.
#[cfg(unix)]
fn my_uid() -> io::Result<u32> {
    if cfg!(target_os = "linux") {
        return Ok(std::fs::metadata("/proc/self")?.uid());
    }
    let probe =
        std::env::temp_dir().join(format!("orbit-uid-{}-{:016x}", std::process::id(), nonce()));
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let uid = f.metadata()?.uid();
    drop(f);
    let _ = std::fs::remove_file(&probe);
    Ok(uid)
}

/// A random number for socket names.
fn nonce() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new()
        .hash_one((std::process::id(), std::time::SystemTime::now()))
}

/// A listening socket, removed when dropped.
pub(crate) struct Bound {
    pub(crate) listener: Listener,
    pub(crate) path: PathBuf,
}

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listen on `<prefix><pid>-<nonce>.sock` in [`dir`], mode 0600.
pub(crate) fn bind(prefix: &str) -> io::Result<Bound> {
    let path = dir()?.join(format!(
        "{prefix}{}-{:016x}.sock",
        std::process::id(),
        nonce()
    ));
    let listener = Listener(RawListener::bind(&path)?);
    let bound = Bound { listener, path };
    #[cfg(unix)]
    std::fs::set_permissions(&bound.path, std::fs::Permissions::from_mode(0o600))?;
    Ok(bound)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn sockets_are_private_and_removed() {
        let bound = bind("test-").unwrap();
        let path = bound.path.clone();
        let dir = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(dir.mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("test-{}-", std::process::id())),
            "{name}"
        );
        drop(bound);
        assert!(!path.exists());
    }
}
