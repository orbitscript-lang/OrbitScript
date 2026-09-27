//! Running an interpreter as a subprocess with a timeout, captured output
//! and, on Unix, exact peak resident set size.
//!
//! Peak RSS comes from `getrusage(RUSAGE_CHILDREN)`, which reports the
//! largest child a process has waited for. To get one number per run, a
//! measured run goes through a copy of this tool (the hidden `__rusage`
//! command) that spawns just that one child, waits for it and writes the
//! result to a file.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// One subprocess invocation.
#[derive(Debug, Clone)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Record peak RSS (Unix only; ignored elsewhere).
    pub measure_rss: bool,
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Normal exit with this code.
    Code(i32),
    /// Killed by a signal (Unix) or ended without a code.
    Signal,
    /// Still running at the deadline, so we killed it.
    TimedOut,
}

impl Exit {
    pub fn describe(self) -> String {
        match self {
            Exit::Code(c) => format!("exit code {c}"),
            Exit::Signal => "killed by a signal".to_string(),
            Exit::TimedOut => "timed out".to_string(),
        }
    }
}

/// Everything we record about a finished process.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall: Duration,
    /// Peak resident set size in KiB, when measured.
    pub peak_rss_kib: Option<u64>,
}

/// Runs `spec`. Standard input is an empty pipe: some suite tests (files.lua)
/// check that seeking on stdin fails, which is only true for a pipe.
pub fn run(spec: &Spec) -> std::io::Result<Outcome> {
    if spec.measure_rss && cfg!(unix) {
        run_measured(spec)
    } else {
        run_plain(spec)
    }
}

fn run_plain(spec: &Spec) -> std::io::Result<Outcome> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    let (exit, wall, stdout, stderr) = spawn_and_wait(cmd, spec.timeout)?;
    Ok(Outcome {
        exit,
        stdout,
        stderr,
        wall,
        peak_rss_kib: None,
    })
}

fn run_measured(spec: &Spec) -> std::io::Result<Outcome> {
    let report = std::env::temp_dir().join(format!(
        "orbit-testkit-rusage-{}-{}",
        std::process::id(),
        REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__rusage")
        .arg(&report)
        .arg(spec.timeout.as_millis().to_string())
        .arg(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd);
    // The helper enforces the timeout on its child; ours is a backstop.
    let (outer, _, stdout, stderr) = spawn_and_wait(cmd, spec.timeout + Duration::from_secs(10))?;
    let text = std::fs::read_to_string(&report).unwrap_or_default();
    let _ = std::fs::remove_file(&report);
    let Some((exit, wall, rss)) = parse_report(&text) else {
        return Err(std::io::Error::other(format!(
            "rusage helper failed ({}): {}",
            outer.describe(),
            String::from_utf8_lossy(&stderr).trim()
        )));
    };
    Ok(Outcome {
        exit,
        stdout,
        stderr,
        wall,
        peak_rss_kib: rss,
    })
}

static REPORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn spawn_and_wait(
    mut cmd: Command,
    timeout: Duration,
) -> std::io::Result<(Exit, Duration, Vec<u8>, Vec<u8>)> {
    let start = Instant::now();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take());
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let exit = wait_with_deadline(&mut child, start + timeout)?;
    let wall = start.elapsed();
    // A test can leave grandchildren (shell pipelines in main.lua) holding
    // our pipes open after a kill, so never wait on the readers forever.
    let grace = Instant::now() + Duration::from_secs(5);
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(grace.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    Ok((exit, wall, collect(stdout), collect(stderr)))
}

fn wait_with_deadline(child: &mut std::process::Child, deadline: Instant) -> std::io::Result<Exit> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().map_or(Exit::Signal, Exit::Code));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Exit::TimedOut);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// The `__rusage REPORT TIMEOUT_MS PROGRAM ARGS...` helper. It runs PROGRAM
/// with our stdio, then writes `EXIT WALL_NANOS PEAK_RSS_KIB` to REPORT.
pub fn rusage_helper(args: &[String]) -> ExitCode {
    let [report, timeout_ms, program, rest @ ..] = args else {
        eprintln!("usage: orbit-testkit __rusage REPORT TIMEOUT_MS PROGRAM [ARGS...]");
        return ExitCode::from(2);
    };
    let timeout = Duration::from_millis(timeout_ms.parse().unwrap_or(u64::MAX / 4));
    let start = Instant::now();
    let exit = Command::new(program)
        .args(rest)
        .spawn()
        .and_then(|mut child| wait_with_deadline(&mut child, start + timeout));
    let wall = start.elapsed();
    let exit = match exit {
        Ok(e) => e,
        Err(e) => {
            eprintln!("orbit-testkit: cannot run {program}: {e}");
            return ExitCode::from(2);
        }
    };
    let rss = children_peak_rss_kib().map_or("-".to_string(), |k| k.to_string());
    let text = format!("{} {} {rss}\n", exit_word(exit), wall.as_nanos());
    match std::fs::write(Path::new(report), text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orbit-testkit: {report}: {e}");
            ExitCode::from(2)
        }
    }
}

fn exit_word(exit: Exit) -> String {
    match exit {
        Exit::Code(c) => c.to_string(),
        Exit::Signal => "signal".to_string(),
        Exit::TimedOut => "timeout".to_string(),
    }
}

fn parse_report(text: &str) -> Option<(Exit, Duration, Option<u64>)> {
    let mut f = text.split_whitespace();
    let exit = match f.next()? {
        "signal" => Exit::Signal,
        "timeout" => Exit::TimedOut,
        code => Exit::Code(code.parse().ok()?),
    };
    let wall = Duration::from_nanos(f.next()?.parse().ok()?);
    let rss = f.next()?.parse().ok();
    Some((exit, wall, rss))
}

#[cfg(unix)]
fn children_peak_rss_kib() -> Option<u64> {
    use nix::sys::resource::{UsageWho, getrusage};
    let max = u64::try_from(getrusage(UsageWho::RUSAGE_CHILDREN).ok()?.max_rss()).ok()?;
    // macOS reports bytes; Linux and the BSDs report KiB.
    Some(if cfg!(target_os = "macos") {
        max / 1024
    } else {
        max
    })
}

#[cfg(not(unix))]
fn children_peak_rss_kib() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_round_trips() {
        let (e, w, r) = parse_report("3 1500000000 2048\n").unwrap();
        assert_eq!(
            (e, w, r),
            (Exit::Code(3), Duration::from_millis(1500), Some(2048))
        );
        let (e, _, r) = parse_report("timeout 5 -").unwrap();
        assert_eq!((e, r), (Exit::TimedOut, None));
        assert_eq!(
            parse_report(&format!("{} 1 1", exit_word(Exit::Signal)))
                .unwrap()
                .0,
            Exit::Signal
        );
        assert!(parse_report("").is_none());
    }

    #[cfg(unix)]
    fn sh(script: &str, timeout: Duration) -> Outcome {
        run(&Spec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: ".".into(),
            timeout,
            measure_rss: false,
        })
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn captures_output_and_exit_code() {
        let out = sh("echo out; echo err >&2; exit 3", Duration::from_secs(10));
        assert_eq!(out.exit, Exit::Code(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        assert_eq!(out.peak_rss_kib, None);
    }

    #[cfg(unix)]
    #[test]
    fn kills_on_timeout() {
        let out = sh("exec sleep 30", Duration::from_millis(200));
        assert_eq!(out.exit, Exit::TimedOut);
        assert!(out.wall < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn peak_rss_of_this_process_is_plausible() {
        // `cargo test` runs this crate's test binary, not orbit-testkit, so
        // the helper can't be exercised here; check the rusage call itself.
        let _child = Command::new("true").status().unwrap();
        let kib = children_peak_rss_kib().unwrap();
        assert!(kib > 0 && kib < 1 << 30, "{kib}");
    }
}
