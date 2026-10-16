//! The terminal side of line editing on Unix, through the `stty` command:
//! the workspace forbids `unsafe`, which the termios calls would need, and
//! `stty` is on every Unix (BusyBox included).

use std::io;
use std::process::{Command, Stdio};

use super::editor::Terminal;

/// The terminal on standard input, with the settings to restore.
pub struct Stty {
    saved: String,
}

/// Run `stty args` on standard input; its output on success.
fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl Stty {
    /// The terminal, if `stty` can read its settings.
    pub fn new() -> Option<Stty> {
        let saved = stty(&["-g"]).filter(|s| !s.is_empty())?;
        Some(Stty { saved })
    }
}

impl Terminal for Stty {
    fn raw(&mut self) -> io::Result<()> {
        // One key at a time, no echo, and Ctrl-C, Ctrl-Z and Ctrl-S arrive
        // as keys. Output processing stays on, so `\n` still starts a line.
        let args = [
            "-icanon", "-echo", "-isig", "-iexten", "-ixon", "-icrnl", "min", "1", "time", "0",
        ];
        match stty(&args) {
            Some(_) => Ok(()),
            None => Err(io::Error::other("stty failed")),
        }
    }

    fn restore(&mut self) {
        stty(&[&self.saved]);
    }

    fn width(&mut self) -> usize {
        stty(&["size"])
            .and_then(|s| s.split_whitespace().nth(1)?.parse().ok())
            .filter(|&c| c > 0)
            .unwrap_or(80)
    }

    fn suspend(&mut self) {
        self.restore();
        let _ = Command::new("kill")
            .args(["-TSTP", &std::process::id().to_string()])
            .status();
        let _ = self.raw();
    }
}
