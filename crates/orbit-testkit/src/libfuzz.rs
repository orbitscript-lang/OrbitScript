//! `fuzz`: differential fuzzing of library functions against the reference
//! interpreter.
//!
//! A generator writes a batch of random calls (one target: `format`,
//! `pattern`, `pack` or `utf8`) into one Lua script, which runs under both
//! interpreters. Each call prints its case number and its results or error,
//! so the first differing output line names the case. Batches are seeded,
//! so `--seed` reproduces a failure exactly.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::diff::{self, Observed};
use crate::process::{self, Spec};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit fuzz --reference PATH --interp PATH [options] TARGET...

Generates random calls for each TARGET (format, pattern, pack, utf8), runs
them under both interpreters and reports the first call whose results or
error differ, with the seed that reproduces it.

options:
  --seed N        seed of the first batch (default: from the clock)
  --batches N     batches per target (default 20)
  --cases N       calls per batch (default 400)
  --timeout SECS  per-batch timeout (default 60)
  --work DIR      where batch scripts go (default target/testkit/fuzz)";

/// The targets, in the order `all` runs them.
const TARGETS: &[&str] = &["format", "pattern", "pack", "utf8"];

pub fn main(args: &[String]) -> Result<bool> {
    let mut reference = None;
    let mut interp = None;
    let mut seed = None;
    let mut batches = 20u64;
    let mut cases = 400usize;
    let mut timeout = Duration::from_secs(60);
    let mut work = util::repo_root().join("target/testkit/fuzz");
    let mut targets = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        let num = |s: String| {
            s.parse::<u64>()
                .map_err(|_| format!("{a}: not a number: {s}"))
        };
        match a.as_str() {
            "--reference" => reference = Some(PathBuf::from(val()?)),
            "--interp" => interp = Some(PathBuf::from(val()?)),
            "--seed" => seed = Some(num(val()?)?),
            "--batches" => batches = num(val()?)?,
            "--cases" => cases = num(val()?)? as usize,
            "--timeout" => timeout = Duration::from_secs(num(val()?)?),
            "--work" => work = PathBuf::from(val()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            "all" => targets.extend(TARGETS.iter().map(|t| t.to_string())),
            t if TARGETS.contains(&t) => targets.push(t.to_string()),
            other => return Err(format!("unknown option or target {other:?}\n\n{USAGE}")),
        }
    }
    let reference =
        util::absolute(&reference.ok_or(format!("--reference is required\n\n{USAGE}"))?)?;
    let interp = util::absolute(&interp.ok_or(format!("--interp is required\n\n{USAGE}"))?)?;
    if targets.is_empty() {
        return Err(format!("name a target\n\n{USAGE}"));
    }
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_secs())
    });
    fs::create_dir_all(&work).map_err(io_err(work.display()))?;
    let work = util::absolute(&work)?;

    let mut ok = true;
    for target in &targets {
        let mut failed = false;
        for b in 0..batches {
            let batch_seed = seed.wrapping_add(b);
            let calls = generate(target, batch_seed, cases);
            let script = work.join(format!("{target}-{batch_seed}.lua"));
            fs::write(&script, render(&calls)).map_err(io_err(script.display()))?;
            let run = |program: &Path| -> Result<Observed> {
                let out = process::run(&Spec {
                    program: program.to_path_buf(),
                    args: vec![script.clone().into()],
                    cwd: work.clone(),
                    timeout,
                    measure_rss: false,
                })
                .map_err(io_err(program.display()))?;
                Ok(diff::observe(&out, program))
            };
            let (r, c) = (run(&reference)?, run(&interp)?);
            if r == c {
                let _ = fs::remove_file(&script);
                continue;
            }
            failed = true;
            report(target, batch_seed, &script, &calls, &r, &c);
            break;
        }
        if failed {
            ok = false;
        } else {
            println!(
                "{target}: {batches} batches of {cases} calls match (seeds {seed}..={})",
                seed.wrapping_add(batches.saturating_sub(1))
            );
        }
    }
    Ok(ok)
}

/// Print the first differing call of a failed batch.
fn report(target: &str, seed: u64, script: &Path, calls: &[String], r: &Observed, c: &Observed) {
    println!(
        "{target}: mismatch in batch seed {seed} ({})",
        script.display()
    );
    let (rl, cl): (Vec<&str>, Vec<&str>) = (r.stdout.lines().collect(), c.stdout.lines().collect());
    let first = (0..rl.len().max(cl.len())).find(|&i| rl.get(i) != cl.get(i));
    match first {
        Some(i) => {
            // Lines start with the case number.
            let case = rl
                .get(i)
                .or(cl.get(i))
                .and_then(|l| l.split('\t').next())
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(call) = case.and_then(|n| calls.get(n)) {
                println!("  call:      {call}");
            }
            let show = |x: Option<&&str>| x.map_or("<missing>".to_string(), |s| format!("{s:?}"));
            println!("  reference: {}", show(rl.get(i)));
            println!("  candidate: {}", show(cl.get(i)));
        }
        None => {
            if let Some(m) = diff::describe_mismatch(r, c) {
                println!("{}", util::indent(&m, "  "));
            }
        }
    }
    println!("  reproduce: --seed {seed} --batches 1 {target}");
}

/// The script for a batch: a helper that prints a call's outcome, then one
/// line per call.
fn render(calls: &[String]) -> String {
    let mut s = String::from(
        "-- Generated by `orbit-testkit fuzz`.\n\
         local function show(v)\n\
         \x20 if type(v) == \"string\" then\n\
         \x20   return '\"' .. string.gsub(v, \"[^%w%p ]\", function(c) return \"\\\\\" .. string.byte(c) end) .. '\"'\n\
         \x20 end\n\
         \x20 return tostring(v)\n\
         end\n\
         local function C(i, ...)\n\
         \x20 local r = table.pack(pcall(...))\n\
         \x20 for k = 1, r.n do r[k] = show(r[k]) end\n\
         \x20 print(i, table.concat(r, \" \", 1, r.n))\n\
         end\n",
    );
    for (i, call) in calls.iter().enumerate() {
        let _ = writeln!(s, "C({i}, {call})");
    }
    s
}

fn generate(target: &str, seed: u64, n: usize) -> Vec<String> {
    let mut g = Gen::new(seed);
    (0..n)
        .map(|_| match target {
            "format" => g.format_call(),
            "pattern" => g.pattern_call(),
            "pack" => g.pack_call(),
            _ => g.utf8_call(),
        })
        .collect()
}

/// A splitmix64 generator plus the case generators.
struct Gen(u64);

impl Gen {
    fn new(seed: u64) -> Self {
        Gen(seed ^ 0x5DEE_CE66_D1CE_4E5B)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True with probability `1/n`.
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

    // ---- values ----------------------------------------------------------

    /// An integer as a Lua expression, favoring edge cases.
    fn int(&mut self) -> String {
        match self.below(8) {
            0 => self
                .pick(&[
                    "0",
                    "1",
                    "-1",
                    "255",
                    "256",
                    "-128",
                    "65535",
                    "2147483647",
                    "-2147483648",
                ])
                .to_string(),
            1 => self
                .pick(&["math.maxinteger", "math.mininteger", "math.maxinteger - 1"])
                .to_string(),
            2 => format!("{}", (self.next() as i64) >> self.below(64)),
            3 => format!("{}", 1i64 << self.below(63)),
            4 => format!("{}", -(1i64 << self.below(63))),
            _ => format!("{}", self.below(2000) as i64 - 1000),
        }
    }

    /// A float as a Lua expression, favoring edge cases and halfway values.
    fn float(&mut self) -> String {
        match self.below(9) {
            0 => self
                .pick(&[
                    "0.0", "-0.0", "1/0", "-1/0", "0.5", "1.5", "2.5", "-2.5", "0.1",
                ])
                .to_string(),
            1 => self
                .pick(&[
                    "1e308",
                    "-1e308",
                    "5e-324",
                    "2.2250738585072014e-308",
                    "1e-310",
                    "1e15",
                    "1e16",
                    "1e100",
                ])
                .to_string(),
            2 => format!("{:?}", f64::from_bits(self.next()))
                .replace("NaN", "0.0")
                .replace("inf", "1/0"),
            3 => format!("{:?}", (self.below(20001) as f64 - 10000.0) / 8.0),
            4 => format!(
                "{:?}",
                (self.below(1_000_000) as f64 + 0.5) * 10f64.powi(self.below(40) as i32 - 20)
            ),
            5 => format!("{}.0", (self.next() as i64) >> self.below(64)),
            _ => {
                let x = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
                let e = self.below(60) as i32 - 30;
                format!("{:?}", x * 10f64.powi(e))
            }
        }
    }

    /// A byte string as a Lua literal, from `alphabet` plus rare others.
    fn bytes_from(&mut self, alphabet: &[u8], max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n)
            .map(|_| {
                if self.one_in(20) {
                    self.below(256) as u8
                } else {
                    *self.pick(alphabet)
                }
            })
            .collect()
    }

    fn string(&mut self, alphabet: &[u8], max: usize) -> String {
        let b = self.bytes_from(alphabet, max);
        lua_str(&b)
    }

    fn any_value(&mut self) -> String {
        match self.below(8) {
            0 | 1 => self.int(),
            2 | 3 => self.float(),
            4 => self.string(b"abc 019%\"\\\n\t\r.-", 12),
            5 => self.pick(&["nil", "true", "false"]).to_string(),
            6 => format!("\"{}\"", self.int()),
            _ => format!("\"{}\"", self.float().replace('/', "")),
        }
    }

    // ---- format ----------------------------------------------------------

    fn format_call(&mut self) -> String {
        let mut fmt = Vec::new();
        let mut args = Vec::new();
        for _ in 0..1 + self.below(3) {
            if self.one_in(3) {
                fmt.extend(self.bytes_from(b"ab ,:|", 3));
            }
            let (spec, arg) = self.format_spec();
            fmt.extend(spec.into_bytes());
            if let Some(a) = arg {
                args.push(a);
            }
        }
        // Sometimes one argument too few or too many.
        if self.one_in(25) {
            args.pop();
        } else if self.one_in(25) {
            args.push(self.any_value());
        }
        let mut call = format!("string.format, {}", lua_str(&fmt));
        for a in args {
            call.push_str(", ");
            call.push_str(&a);
        }
        call
    }

    fn format_spec(&mut self) -> (String, Option<String>) {
        if self.one_in(15) {
            return ("%%".to_string(), None);
        }
        let conv = *self.pick(b"ddiiuoxXcaAeEffgGqss");
        let conv = if self.one_in(40) {
            *self.pick(b"bkzyI")
        } else {
            conv
        };
        let mut spec = String::from("%");
        for _ in 0..self.below(4) {
            spec.push(*self.pick(&['-', '+', ' ', '#', '0']));
        }
        if self.one_in(2) {
            let w = if self.one_in(10) {
                self.below(200)
            } else {
                self.below(30)
            };
            let _ = write!(spec, "{w}");
        }
        if self.one_in(2) {
            spec.push('.');
            if !self.one_in(6) {
                let p = if self.one_in(10) {
                    self.below(200)
                } else {
                    self.below(30)
                };
                let _ = write!(spec, "{p}");
            }
        }
        spec.push(conv as char);
        let arg = match conv {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => match self.below(10) {
                0 => self.float(),
                1 => self.any_value(),
                _ => self.int(),
            },
            b'c' => match self.below(8) {
                0 => self.any_value(),
                _ => format!("{}", self.below(256)),
            },
            b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => match self.below(8) {
                0 => self.int(),
                1 => self.any_value(),
                _ => self.float(),
            },
            _ => self.any_value(),
        };
        (spec, Some(arg))
    }

    // ---- patterns --------------------------------------------------------

    const SUBJECT: &'static [u8] = b"aabbcc019 .-()[]%xyzAB\t";

    fn pattern(&mut self) -> Vec<u8> {
        let mut p = Vec::new();
        if self.one_in(4) {
            p.push(b'^');
        }
        let mut open = 0;
        for _ in 0..1 + self.below(6) {
            match self.below(12) {
                0 if open < 3 => {
                    p.push(b'(');
                    open += 1;
                    if self.one_in(4) {
                        p.push(b')');
                        open -= 1;
                    }
                }
                1 if open > 0 => {
                    p.push(b')');
                    open -= 1;
                }
                2 => {
                    p.push(b'%');
                    p.push(*self.pick(b"adlsuwxpcgADLSUWXPCG.%-(z"));
                }
                3 => {
                    p.push(b'[');
                    if self.one_in(3) {
                        p.push(b'^');
                    }
                    for _ in 0..1 + self.below(3) {
                        match self.below(4) {
                            0 => p.extend_from_slice(self.pick::<&[u8]>(&[
                                b"a-c".as_slice(),
                                b"0-9",
                                b"x-z",
                            ])),
                            1 => {
                                p.push(b'%');
                                p.push(*self.pick(b"adsw]%-"));
                            }
                            _ => p.push(*self.pick(Self::SUBJECT)),
                        }
                    }
                    if !self.one_in(20) {
                        p.push(b']');
                    }
                }
                4 => p.extend_from_slice(self.pick::<&[u8]>(&[
                    b"%b()".as_slice(),
                    b"%bab",
                    b"%b[]",
                    b"%f[%w]",
                    b"%f[%W]",
                    b"%f[a-c]",
                ])),
                5 if self.one_in(3) => {
                    p.push(b'%');
                    p.push(b'1' + self.below(3) as u8);
                }
                6 => p.push(b'.'),
                _ => p.push(*self.pick(b"aabbc019 x-")),
            }
            if self.one_in(3) {
                p.push(*self.pick(b"*+-?"));
            }
        }
        for _ in 0..open {
            if !self.one_in(10) {
                p.push(b')');
            }
        }
        if self.one_in(5) {
            p.push(b'$');
        }
        p
    }

    fn pattern_call(&mut self) -> String {
        let s = self.string(Self::SUBJECT, 16);
        let p = lua_str(&self.pattern());
        let init = if self.one_in(3) {
            format!(", {}", self.below(24) as i64 - 6)
        } else {
            String::new()
        };
        match self.below(5) {
            0 => format!("string.find, {s}, {p}{init}"),
            1 => format!("string.find, {s}, {p}, {}, true", self.below(10) as i64 - 3),
            2 => format!("string.match, {s}, {p}{init}"),
            _ => {
                let repl = match self.below(4) {
                    0 => lua_str(&self.bytes_from(b"x%0%1%2%%-", 6)),
                    1 => "{a = 1, b = false, aa = \"<aa>\", [\"0\"] = 0}".to_string(),
                    2 => "function(...) return select('#', ...) end".to_string(),
                    _ => "function(a) if a == \"a\" then return nil end return a .. a end"
                        .to_string(),
                };
                let n = if self.one_in(3) {
                    format!(", {}", self.below(5) as i64 - 1)
                } else {
                    String::new()
                };
                format!("string.gsub, {s}, {p}, {repl}{n}")
            }
        }
    }

    // ---- pack ------------------------------------------------------------

    fn pack_call(&mut self) -> String {
        let mut fmt = String::new();
        let mut vals = Vec::new();
        for _ in 0..1 + self.below(5) {
            if self.one_in(6) {
                fmt.push(*self.pick(&['<', '>', '=', '!', ' ']));
                if fmt.ends_with('!') && self.one_in(2) {
                    let _ = write!(fmt, "{}", 1 << self.below(5));
                }
            }
            let opt = *self.pick(&[
                "b", "B", "h", "H", "i", "I", "l", "L", "j", "J", "T", "f", "d", "n", "s", "z",
                "x", "Xi", "X!4", "c",
            ]);
            fmt.push_str(opt);
            let sized = matches!(opt, "i" | "I" | "s" | "c");
            if sized && !self.one_in(4) {
                let n = if self.one_in(15) {
                    self.below(40)
                } else {
                    1 + self.below(16)
                };
                let _ = write!(fmt, "{n}");
            }
            vals.push(match opt {
                "f" | "d" | "n" => self.float(),
                "s" | "z" => self.string(b"ab\0c", 6),
                "c" => self.string(b"xyz", 6),
                "x" | "Xi" | "X!4" => continue,
                _ => {
                    if self.one_in(10) {
                        self.any_value()
                    } else {
                        self.int()
                    }
                }
            });
        }
        let f = lua_str(fmt.as_bytes());
        match self.below(4) {
            0 => format!("string.packsize, {f}"),
            1 => {
                let data = self.string(b"\0\x01\x7f\x80\xff ab", 24);
                let pos = if self.one_in(3) {
                    format!(", {}", self.below(12) as i64 - 3)
                } else {
                    String::new()
                };
                format!("string.unpack, {f}, {data}{pos}")
            }
            _ => format!(
                "function() local s = string.pack({f}{}) return #s, s, string.unpack({f}, s) end",
                vals.iter().map(|v| format!(", {v}")).collect::<String>()
            ),
        }
    }

    // ---- utf8 ------------------------------------------------------------

    fn utf8_bytes(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        for _ in 0..self.below(6) {
            match self.below(9) {
                0 => b.push(b'a' + self.below(3) as u8),
                1 => b.extend_from_slice(self.pick::<&[u8]>(&[
                    b"\xC3\xA9".as_slice(),
                    b"\xE2\x82\xAC",
                    b"\xF0\x9F\x98\x80",
                    b"\xF4\x8F\xBF\xBF",
                ])),
                2 => b.extend_from_slice(self.pick::<&[u8]>(&[
                    b"\xED\xA0\x80".as_slice(),
                    b"\xF4\x90\x80\x80",
                    b"\xF8\x88\x80\x80\x80",
                    b"\xFC\x84\x80\x80\x80\x80",
                    b"\xFD\xBF\xBF\xBF\xBF\xBF",
                ])),
                3 => b.extend_from_slice(self.pick::<&[u8]>(&[
                    b"\xC0\x80".as_slice(),
                    b"\xE0\x80\x80",
                    b"\xC2",
                    b"\xE2\x82",
                    b"\x80",
                    b"\xBF",
                    b"\xFE",
                    b"\xFF",
                ])),
                _ => b.push(self.below(128) as u8),
            }
        }
        b
    }

    fn utf8_call(&mut self) -> String {
        let s = lua_str(&self.utf8_bytes());
        let pos = |g: &mut Self| g.below(14) as i64 - 6;
        let lax = if self.one_in(3) { ", true" } else { "" };
        match self.below(6) {
            0 => format!("utf8.len, {s}, {}, {}{lax}", pos(self), pos(self)),
            1 => format!("utf8.len, {s}"),
            2 => format!("utf8.codepoint, {s}, {}, {}{lax}", pos(self), pos(self)),
            3 => format!("utf8.offset, {s}, {}, {}", pos(self), pos(self)),
            4 => {
                let cps: Vec<String> = (0..1 + self.below(3))
                    .map(|_| match self.below(5) {
                        0 => format!("{}", self.below(0x80)),
                        1 => self
                            .pick(&[
                                "0x7FF",
                                "0x800",
                                "0xFFFF",
                                "0x10000",
                                "0x10FFFF",
                                "0x110000",
                                "0xD800",
                                "0x7FFFFFFF",
                                "0x80000000",
                                "-1",
                            ])
                            .to_string(),
                        _ => format!("{}", self.below(0x11_0000)),
                    })
                    .collect();
                format!("utf8.char, {}", cps.join(", "))
            }
            _ => format!(
                "function() local t = {{}} for p, c in utf8.codes({s}{lax}) do t[#t + 1] = p .. \":\" .. c end return table.concat(t, \" \") end"
            ),
        }
    }
}

/// `b` as a Lua string literal with every unusual byte escaped (as three
/// digits, so a following digit can't join the escape).
fn lua_str(b: &[u8]) -> String {
    let mut s = String::from("\"");
    for &c in b {
        if c.is_ascii_alphanumeric() || b" .,:;!?()[]{}+-*/^%#&|~<>=_@$'`".contains(&c) {
            s.push(c as char);
        } else {
            let _ = write!(s, "\\{c:03}");
        }
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(lua_str(b"a\x001"), "\"a\\0001\"");
        assert_eq!(lua_str(b"\"\\\n"), "\"\\034\\092\\010\"");
    }

    #[test]
    fn deterministic() {
        assert_eq!(generate("format", 7, 50), generate("format", 7, 50));
        assert_ne!(generate("pattern", 7, 50), generate("pattern", 8, 50));
    }
}
