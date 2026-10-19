//! Local sockets for the debugger (ADR-12, D11): a per-user directory only
//! its owner can enter, and sockets named so a reused pid never lands on
//! an old name. No TCP.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// The directory the sockets go in: `$XDG_RUNTIME_DIR/orbit`, else
/// `$TMPDIR/orbit` on macOS (already per user), else
/// `$TMPDIR/orbit-<uid>` or `/tmp/orbit-<uid>`. It is created mode 0700
/// and refused if someone else owns it or can enter it.
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

/// This process's user id, from a file it owns.
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
    pub(crate) listener: UnixListener,
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
    let listener = UnixListener::bind(&path)?;
    let bound = Bound { listener, path };
    set_owner_only(&bound.path)?;
    Ok(bound)
}

fn set_owner_only(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
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
