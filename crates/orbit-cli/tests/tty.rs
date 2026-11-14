//! Line editing on a real terminal (a pseudo-terminal): the editor puts
//! the terminal in character-at-a-time mode at the prompt and gives the
//! script its normal mode back, after a line, after an error and around
//! Ctrl-Z, and on the way out.

#![cfg(all(target_os = "linux", feature = "line-editing"))]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use rustix::fs::OFlags;
use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, tcgetattr};

/// `orbit` on a new pseudo-terminal: the child, the terminal's master
/// side for typing, the slave side for reading its modes, and what the
/// child writes.
struct Session {
    child: Child,
    master: File,
    slave: File,
    output: Receiver<u8>,
    seen: Vec<u8>,
    /// How much of `seen` an `expect` has matched.
    matched: usize,
}

impl Session {
    fn start() -> Session {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NOCTTY.bits() as i32)
            .open(name.to_str().unwrap())
            .unwrap();
        // In a process group of its own, which this process's keeps from
        // being orphaned: the system discards a stop signal to an orphaned
        // group, so Ctrl-Z would do nothing.
        let child = Command::new(env!("CARGO_BIN_EXE_orbit"))
            .process_group(0)
            .env_clear()
            .env("TERM", "xterm")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let master = File::from(master);
        let mut reader = master.try_clone().unwrap();
        let (tx, output) = channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || buf[..n].iter().any(|&b| tx.send(b).is_err()) {
                    break;
                }
            }
        });
        Session {
            child,
            master,
            slave,
            output,
            seen: Vec::new(),
            matched: 0,
        }
    }

    /// Wait until the output shows `text` after what the last `expect`
    /// matched.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = text.as_bytes();
        loop {
            let rest = &self.seen[self.matched..];
            if let Some(i) = rest.windows(text.len()).position(|w| w == text) {
                self.matched += i + text.len();
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.output.recv_timeout(left) {
                Ok(b) => self.seen.push(b),
                Err(_) => panic!(
                    "waiting for {:?}; output so far: {:?}",
                    String::from_utf8_lossy(text),
                    String::from_utf8_lossy(&self.seen)
                ),
            }
        }
    }

    /// Wait for a new prompt, at the start of a line (the editor redraws
    /// the prompt as a line is typed, too), and for the editor to be
    /// reading keys.
    fn prompt(&mut self) {
        self.expect("\n\r\x1b[J> ");
        self.wait_canonical(false);
    }

    fn type_keys(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
    }

    /// Whether the terminal is in its normal, line-at-a-time mode.
    fn canonical(&self) -> bool {
        let t = tcgetattr(&self.slave).unwrap();
        t.local_modes
            .contains(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG)
    }

    /// Wait until the terminal is in its normal mode (`true`) or not.
    fn wait_canonical(&self, normal: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.canonical() != normal {
            assert!(Instant::now() < deadline, "terminal never became {normal}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The child's state letter in `/proc` (`T` when stopped).
    fn state(&self) -> char {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).unwrap();
        let after = &stat[stat.rfind(')').unwrap() + 2..];
        after.chars().next().unwrap()
    }
}

#[test]
fn terminal_modes_follow_the_prompt() {
    let mut s = Session::start();
    assert!(s.canonical());
    // At the prompt: one key at a time, no echo, signal keys as keys.
    s.prompt();

    // A script reading input gets the normal mode back.
    s.type_keys("x = io.read()\r");
    s.wait_canonical(true);
    s.type_keys("typed\n");
    s.prompt();
    s.type_keys("print(x)\r");
    s.expect("\ntyped");
    s.prompt();

    // So does the line after an error.
    s.type_keys("error('boom')\r");
    s.expect("boom");
    s.prompt();
    s.type_keys("io.read()\r");
    s.wait_canonical(true);
    s.type_keys("\n");
    s.prompt();

    // Ctrl-Z stops the process with the normal mode back, and continuing
    // it returns to the prompt's mode.
    s.type_keys("\x1a");
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.state() != 'T' {
        assert!(
            Instant::now() < deadline,
            "Ctrl-Z didn't stop the process ({}): {:?}",
            s.state(),
            String::from_utf8_lossy(&s.seen)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.canonical());
    let pid = Pid::from_raw(s.child.id() as i32).unwrap();
    kill_process(pid, Signal::CONT).unwrap();
    s.wait_canonical(false);
    s.type_keys("print(1 + 1)\r");
    s.expect("\n2");
    s.prompt();

    // Ctrl-D on an empty line ends it, the terminal as it was.
    s.type_keys("\x04");
    let status = s.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(s.canonical());
}
