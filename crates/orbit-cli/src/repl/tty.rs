//! The terminal side of line editing on Unix: the terminal's modes
//! (`tcgetattr` and `tcsetattr`), its width (`TIOCGWINSZ`) and stopping
//! on Ctrl-Z (`SIGTSTP`), through `rustix`'s safe interface, so the CLI
//! has no `unsafe` of its own (ADR-6, ADR-7).

use std::io;

use rustix::process::{Signal, getpid, kill_process};
use rustix::termios::{
    InputModes, LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetwinsize,
    tcsetattr,
};

use super::editor::Terminal;

/// The terminal on standard input, with the settings to restore.
pub struct Tty {
    saved: Termios,
}

impl Tty {
    /// The terminal, if standard input is one whose settings can be read.
    pub fn new() -> Option<Tty> {
        let saved = tcgetattr(io::stdin()).ok()?;
        Some(Tty { saved })
    }

    /// Apply `t` to the terminal once the output written so far is sent,
    /// as `stty` does.
    fn set(t: &Termios) -> io::Result<()> {
        Ok(tcsetattr(io::stdin(), OptionalActions::Drain, t)?)
    }
}

impl Terminal for Tty {
    fn raw(&mut self) -> io::Result<()> {
        // One key at a time, no echo, and Ctrl-C, Ctrl-Z and Ctrl-S arrive
        // as keys. Output processing stays on, so `\n` still starts a line.
        let mut t = self.saved.clone();
        t.local_modes -=
            LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG | LocalModes::IEXTEN;
        t.input_modes -= InputModes::IXON | InputModes::ICRNL;
        t.special_codes[SpecialCodeIndex::VMIN] = 1;
        t.special_codes[SpecialCodeIndex::VTIME] = 0;
        Tty::set(&t)
    }

    fn restore(&mut self) {
        let _ = Tty::set(&self.saved);
    }

    fn width(&mut self) -> usize {
        tcgetwinsize(io::stdin())
            .map(|w| usize::from(w.ws_col))
            .ok()
            .filter(|&c| c > 0)
            .unwrap_or(80)
    }

    fn suspend(&mut self) {
        self.restore();
        // Stops here until the shell continues the process.
        let _ = kill_process(getpid(), Signal::TSTP);
        let _ = self.raw();
    }
}
