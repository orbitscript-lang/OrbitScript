//! Small helpers shared by the subcommands.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Error type for the whole tool: a message for the user.
pub type Result<T> = std::result::Result<T, String>;

pub fn io_err(what: impl std::fmt::Display) -> impl FnOnce(io::Error) -> String {
    move |e| format!("{what}: {e}")
}

/// The repository root, found from this crate's manifest directory so the
/// tool works from any current directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives in crates/orbit-testkit")
        .to_path_buf()
}

/// The `orbitc` compiler next to interpreter `interp` (the CLI crate builds
/// both), for the checks that only compile.
pub fn compiler_next_to(interp: &Path) -> PathBuf {
    interp.with_file_name(format!("orbitc{}", std::env::consts::EXE_SUFFIX))
}

/// Makes `path` absolute against the current directory without touching the
/// filesystem, because subprocesses run with a different working directory.
pub fn absolute(path: &Path) -> Result<PathBuf> {
    std::env::current_dir()
        .map(|d| d.join(path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Recursively copies a directory tree.
pub fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Removes and recreates a scratch directory.
pub fn fresh_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).map_err(io_err(dir.display()))?;
    }
    fs::create_dir_all(dir).map_err(io_err(dir.display()))
}

/// Parses an interpreter given as `name=path`, or a bare path named after
/// its file name.
pub fn parse_interp(arg: &str) -> Result<(String, PathBuf)> {
    let (name, path) = match arg.split_once('=') {
        Some((n, p)) if !n.is_empty() => (n.to_string(), PathBuf::from(p)),
        _ => {
            let path = PathBuf::from(arg);
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| format!("cannot name interpreter {arg:?}"))?;
            (name, path)
        }
    };
    if !path.is_file() {
        return Err(format!("interpreter {} not found", path.display()));
    }
    Ok((name, absolute(&path)?))
}

/// The last `n` lines of some output, for failure reports.
pub fn tail(bytes: &[u8], n: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn indent(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}\n")).collect()
}

pub fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

pub fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 {
        f64::NAN
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

pub fn geomean(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() || xs.iter().any(|x| x.is_nan() || *x <= 0.0) {
        return None;
    }
    Some((xs.iter().map(|x| x.ln()).sum::<f64>() / xs.len() as f64).exp())
}

/// Quotes a string as a JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A JSON number, or `null` for values JSON can't carry.
pub fn json_num(x: Option<f64>) -> String {
    match x {
        Some(v) if v.is_finite() => format!("{v}"),
        _ => "null".to_string(),
    }
}

/// Splits a manifest line into whitespace-separated fields, dropping a
/// trailing `# comment`, which is returned separately.
pub fn manifest_fields(line: &str) -> (Vec<&str>, &str) {
    let (body, comment) = match line.find('#') {
        Some(i) => (&line[..i], line[i + 1..].trim()),
        None => (line, ""),
    };
    (body.split_whitespace().collect(), comment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\u0001""#);
        assert_eq!(json_num(Some(1.5)), "1.5");
        assert_eq!(json_num(Some(f64::NAN)), "null");
        assert_eq!(json_num(None), "null");
    }

    #[test]
    fn stats() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), 2.5);
        let g = geomean(&[1.0, 4.0]).unwrap();
        assert!((g - 2.0).abs() < 1e-12);
        assert_eq!(geomean(&[]), None);
        assert_eq!(geomean(&[1.0, 0.0]), None);
    }

    #[test]
    fn manifest_lines() {
        let (f, c) = manifest_fields("big.lua  yields=b,a  # needs a coroutine");
        assert_eq!(f, ["big.lua", "yields=b,a"]);
        assert_eq!(c, "needs a coroutine");
        assert_eq!(manifest_fields("   # only a comment").0.len(), 0);
    }

    #[test]
    fn tails_output() {
        assert_eq!(tail(b"a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail(b"", 3), "");
    }
}
