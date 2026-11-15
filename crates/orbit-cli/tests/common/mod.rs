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

/// A command that runs `program`, under the command `ORBIT_TEST_WRAPPER`
/// names when it is set (words split at spaces), as tests/memcheck/run.sh
/// runs programs under valgrind.
#[allow(dead_code)] // not every test program runs one wrapped
pub fn wrapped(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    use std::process::Command;
    let wrapper = std::env::var("ORBIT_TEST_WRAPPER").unwrap_or_default();
    let mut words = wrapper.split_whitespace();
    let mut command = match words.next() {
        Some(first) => Command::new(first),
        None => return Command::new(program),
    };
    command.args(words).arg(program);
    command
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
