//! Differential tests against the reference `lua5.5`: thousands of generated calls to
//! each library piece, run through both implementations, must give identical results
//! and error messages.
//!
//! The reference interpreter is `$LUA55`, or `target/reference-lua/bin/lua5.5` as built
//! by `scripts/build-reference-lua.sh`. Without it the test prints a note and passes.
//! `ORBIT_REF_CASES` sets the number of cases per group (default 3000).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use orbit_stdlib::format::{FormatArgs, Literal};
use orbit_stdlib::pack::{PackArgs, Unpacked};
use orbit_stdlib::pattern::{self, Capture, Captures, GMatch, GsubMatch, Replacement};
use orbit_stdlib::random::{RandomArgs, RandomValue, Xoshiro256};
use orbit_stdlib::{LibError, float_to_str};

fn reference_lua() -> Option<PathBuf> {
    let path = match std::env::var_os("LUA55") {
        Some(p) => PathBuf::from(p),
        None => {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/reference-lua/bin/lua5.5")
        }
    };
    path.exists().then_some(path)
}

fn cases() -> usize {
    std::env::var("ORBIT_REF_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000)
}

/// Lua helpers: serialization of results, and decoding of test inputs.
const PRELUDE: &str = r##"
local function hex(s) return (s:gsub(".", function(c) return string.format("%02x", c:byte()) end)) end
local function F(h) return (string.unpack("<d", (h:gsub("..", function(x) return string.char(tonumber(x, 16)) end)))) end
local function ser1(v)
  local t = math.type(v) or type(v)
  if t == "string" then return "s" .. hex(v)
  elseif t == "integer" then return "i" .. string.format("%d", v)
  elseif t == "float" then return "f" .. hex(string.pack("<d", v))
  elseif t == "nil" then return "n"
  elseif t == "boolean" then return "b" .. tostring(v)
  else return "?" .. t end
end
local function ser(ok, ...)
  if not ok then
    -- Positions and function names come from the VM, not from these functions.
    local m = (...):gsub("^[^:]*:%d+: ", "")
    m = m:gsub("^bad argument (#%d+) to '[^']*'", "bad argument %1")
    return "E" .. hex(m)
  end
  local parts = {}
  for i = 1, select("#", ...) do parts[#parts + 1] = ser1((select(i, ...))) end
  return table.concat(parts, " ")
end
local function gm(s, p, init)
  local out = {}
  for a, b in string.gmatch(s, p, init) do
    out[#out + 1] = ser1(a) .. "," .. ser1(b)
    if #out > 50 then break end
  end
  return table.concat(out, ";")
end
local function repl(...)
  local a = ...
  if a == "a" then return false end
  return "[" .. select("#", ...) .. "]"
end
local function P(...) io.write(ser(...), "\n") end
"##;

/// A value passed to or returned from a library call.
#[derive(Debug, Clone)]
enum V {
    S(Vec<u8>),
    I(i64),
    F(f64),
    Nil,
    B(bool),
}

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, c| {
        let _ = write!(s, "{c:02x}");
        s
    })
}

fn ser1(v: &V) -> String {
    match v {
        V::S(s) => format!("s{}", hex(s)),
        V::I(i) => format!("i{i}"),
        V::F(f) => format!("f{}", hex(&f.to_le_bytes())),
        V::Nil => "n".into(),
        V::B(b) => format!("b{b}"),
    }
}

fn ser(r: Result<Vec<V>, LibError>) -> String {
    match r {
        Ok(vs) => vs.iter().map(ser1).collect::<Vec<_>>().join(" "),
        Err(e) => {
            let m = match e {
                LibError::Message(m) => m,
                LibError::Arg { arg, msg } => {
                    let mut m = format!("bad argument #{arg} (").into_bytes();
                    m.extend_from_slice(&msg);
                    m.push(b')');
                    m
                }
            };
            format!("E{}", hex(&m))
        }
    }
}

/// A Lua expression for a value.
fn lua(v: &V) -> String {
    match v {
        V::S(s) => {
            let mut o = String::from("\"");
            for c in s {
                let _ = write!(o, "\\x{c:02x}");
            }
            o.push('"');
            o
        }
        V::I(i64::MIN) => "math.mininteger".into(),
        V::I(i) => format!("{i}"),
        V::F(f) => format!("F\"{}\"", hex(&f.to_le_bytes())),
        V::Nil => "nil".into(),
        V::B(b) => b.to_string(),
    }
}

/// Test-case generation, driven by the generator under test (its correctness is checked
/// separately).
struct Gen(Xoshiro256);

impl Gen {
    fn below(&mut self, n: usize) -> usize {
        (self.0.next_u64() % n as u64) as usize
    }
    fn chance(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn bytes_from(&mut self, alphabet: &[u8], max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| *self.pick(alphabet)).collect()
    }
    fn int(&mut self) -> i64 {
        match self.below(6) {
            0 => *self.pick(&[0, 1, -1, i64::MIN, i64::MAX, 255, 256, -128, 65535]),
            1 => self.0.next_u64() as i64,
            2 => self.below(1000) as i64 - 500,
            3 => (self.0.next_u64() >> self.below(64)) as i64,
            _ => -((self.0.next_u64() >> self.below(64)) as i64),
        }
    }
    fn float(&mut self) -> f64 {
        match self.below(7) {
            0 => *self.pick(&[
                0.0,
                -0.0,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NAN,
                -f64::NAN,
                0.5,
                0.125,
                2.5,
                1e15,
                1e16,
                1e21,
                123456789.0,
                0.1,
                1.0 / 3.0,
                5e-324,
                f64::MAX,
                f64::MIN_POSITIVE,
                9_223_372_036_854_775_808.0,
            ]),
            1 => f64::from_bits(self.0.next_u64()),
            2 => {
                (self.below(20001) as f64 - 10000.0)
                    / *self.pick(&[1.0, 2.0, 4.0, 8.0, 10.0, 100.0, 1000.0, 3.0])
            }
            3 => {
                let m = (self.below(2_000_000) as f64 - 1e6) / 1000.0;
                m * 10f64.powi(self.below(80) as i32 - 40)
            }
            4 => self.int() as f64,
            _ => {
                let e = self.below(40) as i32 - 20;
                self.below(1000) as f64 * 2f64.powi(e) + 0.5
            }
        }
    }
}

/// Runs the Lua program and returns its output lines.
fn run_lua(lua: &PathBuf, what: &str, body: &str) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("orbit-stdlib-ref-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{what}.lua"));
    std::fs::write(&file, format!("{PRELUDE}\n{body}")).unwrap();
    let out = Command::new(lua).arg(&file).output().expect("run lua5.5");
    assert!(
        out.status.success(),
        "lua5.5 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&file);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Compares our results with the reference and reports the first mismatches.
fn compare(what: &str, calls: &[String], ours: &[String], theirs: &[String]) {
    assert_eq!(ours.len(), theirs.len(), "{what}: line count");
    let bad: Vec<_> = (0..ours.len()).filter(|&i| ours[i] != theirs[i]).collect();
    for &i in bad.iter().take(10) {
        eprintln!(
            "{what} mismatch: {}\n  orbit: {}\n  lua:   {}",
            calls[i], ours[i], theirs[i]
        );
    }
    assert!(
        bad.is_empty(),
        "{what}: {} of {} cases differ",
        bad.len(),
        ours.len()
    );
    eprintln!("{what}: {} cases agree with lua5.5", ours.len());
}

fn cap_value(s: &[u8], c: &Capture) -> V {
    match c {
        Capture::Str { .. } => V::S(c.bytes(s).unwrap().to_vec()),
        Capture::Position(p) => V::I(*p),
    }
}

fn cap_values(s: &[u8], c: &Captures) -> Vec<V> {
    c.as_slice().iter().map(|c| cap_value(s, c)).collect()
}

fn gen_pattern(g: &mut Gen) -> Vec<u8> {
    const PIECES: &[&[u8]] = &[
        b"a", b"b", b"x", b".", b"%a", b"%d", b"%s", b"%w", b"%p", b"%x", b"%u", b"%l", b"%c",
        b"%g", b"%A", b"%S", b"%z", b"%.", b"%%", b"[ab]", b"[^a]", b"[a-c]", b"[%a_]", b"[]]",
        b"[^]a]", b"[a-]", b"[%]]", b"(", b")", b"()", b"%b()", b"%bab", b"%f[%w]", b"%f[^a]",
        b"%1", b"%2", b"$", b"^", b"-", b"*", b"+", b"?", b"*", b"+", b"-", b"?", b" ", b"\0",
    ];
    const BAD: &[&[u8]] = &[
        b"%", b"[a", b"%b", b"%fa", b"[", b"%b(", b"(()", b"%9", b")",
    ];
    let mut p = Vec::new();
    if g.chance(4) {
        p.push(b'^');
    }
    for _ in 0..g.below(7) {
        let piece = *g.pick(PIECES);
        p.extend_from_slice(piece);
    }
    if g.chance(30) {
        let piece = *g.pick(BAD);
        p.extend_from_slice(piece);
    }
    p
}

/// The Rust side of the Lua `repl` function in the prelude.
fn gsub_callback(m: &GsubMatch<'_, '_>, out: &mut Vec<u8>) -> Result<bool, LibError> {
    let caps = m.captures()?;
    if let Some(Capture::Str { .. }) = caps.as_slice().first()
        && caps.as_slice()[0].bytes(m.subject()) == Some(b"a")
    {
        return Ok(false);
    }
    out.extend_from_slice(format!("[{}]", caps.len()).as_bytes());
    Ok(true)
}

#[test]
fn patterns_match_reference() {
    let Some(lua_path) = reference_lua() else {
        eprintln!("skipped: reference lua5.5 not found (scripts/build-reference-lua.sh)");
        return;
    };
    let mut g = Gen(Xoshiro256::with_seed(1, 0));
    let (mut calls, mut ours, mut body) = (Vec::new(), Vec::new(), String::new());
    for _ in 0..cases() {
        let s = g.bytes_from(b"aabbx()% .-\0\n1]", 12);
        let p = gen_pattern(&mut g);
        let init = g.below(20) as i64 - 6;
        let (ls, lp) = (lua(&V::S(s.clone())), lua(&V::S(p.clone())));
        // find (plain and not), match
        let plain = g.chance(5);
        let call = format!("string.find({ls}, {lp}, {init}, {plain})");
        let r = pattern::find(&s, &p, init, plain).map(|f| match f {
            None => vec![V::Nil],
            Some(f) => {
                let mut v = vec![V::I(f.start as i64 + 1), V::I(f.end as i64)];
                v.extend(cap_values(&s, &f.captures));
                v
            }
        });
        ours.push(ser(r));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);

        let call = format!("string.match({ls}, {lp}, {init})");
        let r = pattern::match_(&s, &p, init).map(|c| match c {
            None => vec![V::Nil],
            Some(c) => cap_values(&s, &c),
        });
        ours.push(ser(r));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);

        // gmatch: all matches, first two captures of each
        let call = format!("gm({ls}, {lp}, {init})");
        let mut gm = GMatch::new(s.len(), init);
        let mut items = Vec::new();
        let r = loop {
            match gm.next(&s, &p) {
                Err(e) => break Err(e),
                Ok(None) => break Ok(()),
                Ok(Some(c)) => {
                    let v = cap_values(&s, &c);
                    let a = v.first().cloned().unwrap_or(V::Nil);
                    let b = v.get(1).cloned().unwrap_or(V::Nil);
                    items.push(format!("{},{}", ser1(&a), ser1(&b)));
                    if items.len() > 50 {
                        break Ok(());
                    }
                }
            }
        };
        ours.push(ser(r.map(|()| vec![V::S(items.join(";").into_bytes())])));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);

        // gsub with a string or a function
        let max = if g.chance(3) {
            Some(g.below(4) as i64)
        } else {
            None
        };
        let lmax = max.map_or("nil".into(), |m| m.to_string());
        let (call, r) = if g.chance(2) {
            const REPLS: &[&[u8]] = &[b"<%0>", b"%1-%%", b"x", b"%2", b"%"];
            let r_str = *g.pick(REPLS);
            let call = format!(
                "string.gsub({ls}, {lp}, {}, {lmax})",
                lua(&V::S(r_str.to_vec()))
            );
            (
                call,
                pattern::gsub::<LibError>(&s, &p, Replacement::Str(r_str), max),
            )
        } else {
            let call = format!("string.gsub({ls}, {lp}, repl, {lmax})");
            let mut f = gsub_callback;
            (call, pattern::gsub(&s, &p, Replacement::Call(&mut f), max))
        };
        let r = r.map(|(out, n)| vec![V::S(out.unwrap_or_else(|| s.clone())), V::I(n)]);
        ours.push(ser(r));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);
    }
    let theirs = run_lua(&lua_path, "patterns", &body);
    compare("patterns", &calls, &ours, &theirs);
}

/// `string.format` arguments as test values.
struct FArgs(Vec<V>);

impl FormatArgs for FArgs {
    type Error = LibError;
    fn top(&self) -> usize {
        self.0.len() + 1
    }
    fn integer(&mut self, arg: usize) -> Result<i64, LibError> {
        match &self.0[arg - 2] {
            V::I(i) => Ok(*i),
            V::F(f)
                if f.fract() == 0.0
                    && *f >= -9_223_372_036_854_775_808.0
                    && *f < 9_223_372_036_854_775_808.0 =>
            {
                Ok(*f as i64)
            }
            V::F(_) => Err(LibError::Arg {
                arg,
                msg: b"number has no integer representation".to_vec(),
            }),
            _ => unreachable!("generator passes numbers"),
        }
    }
    fn number(&mut self, arg: usize) -> Result<f64, LibError> {
        match &self.0[arg - 2] {
            V::I(i) => Ok(*i as f64),
            V::F(f) => Ok(*f),
            _ => unreachable!("generator passes numbers"),
        }
    }
    fn tostring(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, LibError> {
        Ok(match &self.0[arg - 2] {
            V::S(s) => Cow::Borrowed(s),
            V::I(i) => Cow::Owned(i.to_string().into_bytes()),
            V::F(f) => {
                let mut v = Vec::new();
                float_to_str(*f, &mut v);
                Cow::Owned(v)
            }
            V::Nil => Cow::Borrowed(b"nil"),
            V::B(b) => Cow::Owned(b.to_string().into_bytes()),
        })
    }
    fn literal(&mut self, arg: usize) -> Literal<'_> {
        match &self.0[arg - 2] {
            V::S(s) => Literal::Str(s),
            V::I(i) => Literal::Int(*i),
            V::F(f) => Literal::Float(*f),
            V::Nil | V::B(_) => Literal::NilOrBool,
        }
    }
    fn pointer(&mut self, _arg: usize) -> Option<usize> {
        None
    }
}

#[test]
fn format_matches_reference() {
    let Some(lua_path) = reference_lua() else {
        eprintln!("skipped: reference lua5.5 not found (scripts/build-reference-lua.sh)");
        return;
    };
    let mut g = Gen(Xoshiro256::with_seed(2, 0));
    let (mut calls, mut ours, mut body) = (Vec::new(), Vec::new(), String::new());
    for _ in 0..cases() {
        let mut fmt = Vec::new();
        let mut args = Vec::new();
        for _ in 0..1 + g.below(3) {
            fmt.extend(g.bytes_from(b"ab %\n", 2).iter().filter(|&&c| c != b'%'));
            fmt.push(b'%');
            if g.chance(20) {
                fmt.push(b'%');
                continue;
            }
            for _ in 0..g.below(3) {
                fmt.push(*g.pick(b"-+ #0"));
            }
            if g.chance(2) {
                let max = if g.chance(30) { 200 } else { 30 };
                fmt.extend(g.below(max).to_string().bytes());
            }
            if g.chance(2) {
                fmt.push(b'.');
                if !g.chance(4) {
                    let max = if g.chance(30) { 150 } else { 25 };
                    fmt.extend(g.below(max).to_string().bytes());
                }
            }
            let conv = *g.pick(b"ddiuoxXcaAeEfgGgfeaqsspyF");
            fmt.push(conv);
            args.push(match conv {
                b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'c' => {
                    if g.chance(8) {
                        V::F(g.float())
                    } else {
                        V::I(g.int())
                    }
                }
                b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => {
                    if g.chance(8) {
                        V::I(g.int())
                    } else {
                        V::F(g.float())
                    }
                }
                b'p' => V::I(1),
                _ => match g.below(6) {
                    0 => V::I(g.int()),
                    1 => V::F(g.float()),
                    2 => V::Nil,
                    3 => V::B(g.chance(2)),
                    _ => V::S(g.bytes_from(b"ab\"\\\n\r\0\x011\x7f\xff", 8)),
                },
            });
        }
        if g.chance(40) {
            args.pop(); // missing argument
        }
        let largs: Vec<String> = args.iter().map(lua).collect();
        let call = format!(
            "string.format({}{}{})",
            lua(&V::S(fmt.clone())),
            if args.is_empty() { "" } else { ", " },
            largs.join(", ")
        );
        let r = orbit_stdlib::format::format(&fmt, &mut FArgs(args)).map(|s| vec![V::S(s)]);
        ours.push(ser(r));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);
    }
    let theirs = run_lua(&lua_path, "format", &body);
    compare("format", &calls, &ours, &theirs);
}

#[test]
fn tostring_matches_reference() {
    let Some(lua_path) = reference_lua() else {
        eprintln!("skipped: reference lua5.5 not found (scripts/build-reference-lua.sh)");
        return;
    };
    let mut g = Gen(Xoshiro256::with_seed(3, 0));
    let (mut calls, mut ours, mut body) = (Vec::new(), Vec::new(), String::new());
    for _ in 0..cases() * 3 {
        let x = g.float();
        let call = format!("tostring({})", lua(&V::F(x)));
        let mut s = Vec::new();
        float_to_str(x, &mut s);
        ours.push(ser(Ok(vec![V::S(s)])));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);
    }
    let theirs = run_lua(&lua_path, "tostring", &body);
    compare("tostring", &calls, &ours, &theirs);
}

/// `string.pack` arguments as test values.
struct PArgs(Vec<V>);

impl PackArgs for PArgs {
    type Error = LibError;
    fn integer(&mut self, arg: usize) -> Result<i64, LibError> {
        match self.0.get(arg - 2) {
            Some(V::I(i)) => Ok(*i),
            _ => Err(LibError::Arg {
                arg,
                msg: b"number expected".to_vec(),
            }),
        }
    }
    fn number(&mut self, arg: usize) -> Result<f64, LibError> {
        match self.0.get(arg - 2) {
            Some(V::F(f)) => Ok(*f),
            Some(V::I(i)) => Ok(*i as f64),
            _ => Err(LibError::Arg {
                arg,
                msg: b"number expected".to_vec(),
            }),
        }
    }
    fn string(&mut self, arg: usize) -> Result<Cow<'_, [u8]>, LibError> {
        match self.0.get(arg - 2) {
            Some(V::S(s)) => Ok(Cow::Borrowed(s)),
            _ => Err(LibError::Arg {
                arg,
                msg: b"string expected".to_vec(),
            }),
        }
    }
}

#[test]
fn pack_matches_reference() {
    let Some(lua_path) = reference_lua() else {
        eprintln!("skipped: reference lua5.5 not found (scripts/build-reference-lua.sh)");
        return;
    };
    let mut g = Gen(Xoshiro256::with_seed(4, 0));
    let (mut calls, mut ours, mut body) = (Vec::new(), Vec::new(), String::new());
    // (option text, kind of argument: i = integer of `size` bytes, u = unsigned, f, s, -)
    const OPTS: &[(&str, char, u32)] = &[
        ("b", 'i', 1),
        ("B", 'u', 1),
        ("h", 'i', 2),
        ("H", 'u', 2),
        ("l", 'i', 8),
        ("L", 'u', 8),
        ("j", 'i', 8),
        ("J", 'u', 8),
        ("T", 'u', 8),
        ("i", 'i', 4),
        ("I", 'u', 4),
        ("i3", 'i', 3),
        ("I5", 'u', 5),
        ("i16", 'i', 16),
        ("I9", 'u', 9),
        ("i12", 'i', 12),
        ("i1", 'i', 1),
        ("f", 'f', 0),
        ("d", 'f', 0),
        ("n", 'f', 0),
        ("s", 's', 0),
        ("s1", 's', 0),
        ("s2", 's', 0),
        ("z", 's', 0),
        ("c3", 's', 0),
        ("c0", 's', 0),
        ("x", '-', 0),
        ("Xi", '-', 0),
        ("Xh", '-', 0),
        ("Xn", '-', 0),
        ("<", '-', 0),
        (">", '-', 0),
        ("=", '-', 0),
        ("!", '-', 0),
        ("!2", '-', 0),
        ("!4", '-', 0),
        ("!3", '-', 0),
        ("!16", '-', 0),
        ("!8", '-', 0),
        (" ", '-', 0),
        ("i17", 'i', 4),
        ("i0", 'i', 4),
        ("c", 's', 0),
        ("Xc2", '-', 0),
        ("q", '-', 0),
        ("Xz", '-', 0),
    ];
    for _ in 0..cases() {
        let mut fmt = String::new();
        let mut args = Vec::new();
        for _ in 0..g.below(6) {
            let &(o, kind, size) = g.pick(OPTS);
            fmt.push_str(o);
            let bits = size * 8;
            match kind {
                'i' => {
                    let v = if bits < 64 && !g.chance(10) {
                        let half = 1i128 << (bits - 1);
                        (g.0.next_u64() as i128 % (2 * half) - half) as i64
                    } else {
                        g.int()
                    };
                    args.push(V::I(v));
                }
                'u' => {
                    let v = if bits < 64 && !g.chance(10) {
                        (g.0.next_u64() % (1u64 << bits)) as i64
                    } else {
                        g.int()
                    };
                    args.push(V::I(v));
                }
                'f' => args.push(V::F(g.float())),
                's' => args.push(V::S(g.bytes_from(b"ab\0c", 5))),
                _ => {}
            }
        }
        let lfmt = lua(&V::S(fmt.clone().into_bytes()));
        let largs: Vec<String> = args.iter().map(lua).collect();
        let call = format!(
            "string.pack({lfmt}{}{})",
            if args.is_empty() { "" } else { ", " },
            largs.join(", ")
        );
        let packed = orbit_stdlib::pack::pack(fmt.as_bytes(), &mut PArgs(args));
        ours.push(ser(packed.clone().map(|s| vec![V::S(s)])));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call.clone());

        let call2 = format!("string.packsize({lfmt})");
        ours.push(ser(
            orbit_stdlib::pack::packsize(fmt.as_bytes()).map(|n| vec![V::I(n)])
        ));
        let _ = writeln!(body, "P(pcall(function() return {call2} end))");
        calls.push(call2);

        // unpack the packed data (or random bytes) at a random position
        let data = match packed {
            Ok(d) if !g.chance(4) => d,
            _ => g.bytes_from(b"\0\x01\x7f\x80\xffab", 24),
        };
        let init = if g.chance(3) {
            g.below(8) as i64 - 3
        } else {
            1
        };
        let call3 = format!(
            "string.unpack({lfmt}, {}, {init})",
            lua(&V::S(data.clone()))
        );
        let r = orbit_stdlib::pack::unpack(fmt.as_bytes(), &data, init).map(|(vs, next)| {
            let mut out: Vec<V> = vs
                .iter()
                .map(|v| match *v {
                    Unpacked::Int(i) => V::I(i),
                    Unpacked::Float(f) => V::F(f),
                    Unpacked::Str { start, end } => V::S(data[start..end].to_vec()),
                })
                .collect();
            out.push(V::I(next));
            out
        });
        ours.push(ser(r));
        let _ = writeln!(body, "P(pcall(function() return {call3} end))");
        calls.push(call3);
    }
    let theirs = run_lua(&lua_path, "pack", &body);
    compare("pack", &calls, &ours, &theirs);
}

#[test]
fn random_matches_reference() {
    let Some(lua_path) = reference_lua() else {
        eprintln!("skipped: reference lua5.5 not found (scripts/build-reference-lua.sh)");
        return;
    };
    let mut g = Gen(Xoshiro256::with_seed(5, 0));
    let (mut calls, mut ours, mut body) = (Vec::new(), Vec::new(), String::new());
    let mut r = Xoshiro256::with_seed(0, 0);
    for i in 0..cases() {
        if i % 50 == 0 {
            let (a, b) = (g.int(), g.int());
            let call = format!("math.randomseed({}, {})", lua(&V::I(a)), lua(&V::I(b)));
            let (x, y) = r.seed(a, b);
            ours.push(ser(Ok(vec![V::I(x), V::I(y)])));
            let _ = writeln!(body, "P(pcall(function() return {call} end))");
            calls.push(call);
        }
        let (call, args) = match g.below(4) {
            0 => ("math.random()".to_owned(), RandomArgs::None),
            1 => {
                let m = if g.chance(3) { 0 } else { g.int() };
                (format!("math.random({})", lua(&V::I(m))), RandomArgs::Up(m))
            }
            _ => {
                let (a, b) = if g.chance(2) {
                    let a = g.below(100) as i64 - 50;
                    (a, a + g.below(60) as i64 - 5)
                } else {
                    (g.int(), g.int())
                };
                (
                    format!("math.random({}, {})", lua(&V::I(a)), lua(&V::I(b))),
                    RandomArgs::Range(a, b),
                )
            }
        };
        let v = r.random(args).map(|v| match v {
            RandomValue::Int(i) => vec![V::I(i)],
            RandomValue::Float(f) => vec![V::F(f)],
        });
        ours.push(ser(v));
        let _ = writeln!(body, "P(pcall(function() return {call} end))");
        calls.push(call);
    }
    // The first call must follow a known seed.
    let body = format!("math.randomseed(0, 0)\n{body}");
    let theirs = run_lua(&lua_path, "random", &body);
    compare("random", &calls, &ours, &theirs);
}
