//! Expected-pass lists: the set of tests an interpreter is known to pass.
//!
//! CI fails when a listed test stops passing (a regression) and reports
//! tests that pass but are not listed yet, so the list only moves forward.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::{Result, io_err};

#[derive(Debug)]
pub struct Expected {
    path: PathBuf,
    names: BTreeSet<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Listed as passing, but failed this run.
    pub regressions: Vec<String>,
    /// Passed this run, but not listed yet.
    pub newly_passing: Vec<String>,
}

impl Expected {
    /// Loads a list. A missing file is an empty list.
    pub fn load(path: &Path) -> Result<Self> {
        let names = match fs::read_to_string(path) {
            Ok(text) => parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Expected {
            path: path.to_path_buf(),
            names,
        })
    }

    /// Compares one run's results. Tests that did not run this time (for
    /// example because of a name filter) are never counted as regressions.
    pub fn check<'a>(&self, results: impl IntoIterator<Item = (&'a str, bool)>) -> Verdict {
        let mut v = Verdict::default();
        for (name, passed) in results {
            match (self.names.contains(name), passed) {
                (true, false) => v.regressions.push(name.to_string()),
                (false, true) => v.newly_passing.push(name.to_string()),
                _ => {}
            }
        }
        v
    }

    /// Rewrites the list from this run's results, keeping entries for tests
    /// that did not run.
    pub fn update<'a>(
        &mut self,
        header: &str,
        results: impl IntoIterator<Item = (&'a str, bool)>,
    ) -> Result<()> {
        for (name, passed) in results {
            if passed {
                self.names.insert(name.to_string());
            } else {
                self.names.remove(name);
            }
        }
        let mut text = String::new();
        for line in header.lines() {
            text.push_str("# ");
            text.push_str(line);
            text.push('\n');
        }
        for name in &self.names {
            text.push_str(name);
            text.push('\n');
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir.display()))?;
        }
        fs::write(&self.path, text).map_err(io_err(self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn parse(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_results() {
        let e = Expected {
            path: PathBuf::new(),
            names: parse("# header\nstrings.lua\nmath.lua  # note\n"),
        };
        let v = e.check([
            ("strings.lua", true),
            ("math.lua", false),
            ("pm.lua", true),
            ("gc.lua", false),
        ]);
        assert_eq!(v.regressions, ["math.lua"]);
        assert_eq!(v.newly_passing, ["pm.lua"]);
    }

    #[test]
    fn update_round_trips() {
        let dir = std::env::temp_dir().join(format!("orbit-testkit-{}", std::process::id()));
        let path = dir.join("expected.txt");
        let mut e = Expected::load(&path).unwrap();
        e.update("line one\nline two", [("b.lua", true), ("a.lua", true)])
            .unwrap();
        e.update("h", [("b.lua", false)]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# h\na.lua\n");
        assert_eq!(Expected::load(&path).unwrap().names.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
