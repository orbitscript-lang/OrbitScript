//! Shared by the tests of the `orbit` and `orbitc` programs.

use std::path::PathBuf;

/// The program `name` under test: Cargo's own build of it (`built`, from
/// `env!("CARGO_BIN_EXE_<name>")`), or, when `ORBIT_BIN_DIR` is set, the one
/// in that directory, which is how the suite runs against a layout
/// `./build.sh` made (the shared one links liborbit).
pub fn bin(name: &str, built: &str) -> PathBuf {
    match std::env::var_os("ORBIT_BIN_DIR") {
        Some(dir) => PathBuf::from(dir).join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
        None => PathBuf::from(built),
    }
}

/// A program's output as text with `\n` line ends. Standard output and
/// error are text streams, which end lines with `\r\n` on Windows; the
/// tests that check those bytes look at them directly.
#[allow(dead_code)] // not every test program reads output
pub fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if cfg!(windows) {
        s.replace("\r\n", "\n")
    } else {
        s.into_owned()
    }
}
