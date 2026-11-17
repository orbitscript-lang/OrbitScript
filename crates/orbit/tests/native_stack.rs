//! The native stack a call back into script code takes, and the budget that
//! keeps a deep nest of such calls from overflowing the thread's stack
//! ([`Config::native_stack_limit`]).
//!
//! `bytes_per_level` measures how much native stack one more level of each
//! kind of nested call takes, in a release build, and fails when a level
//! takes more than the record in `tests/native-stack/expected.txt` allows
//! (CI runs it, `tests/README.md`, "Native stack per nested call").

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::Rc;

use orbit::{Chunk, Config, Error, Function, Runtime};

/// Nesting until the native stack budget runs out on a thread with only
/// 128 KiB of stack ends in a script error, never a crash.
#[test]
fn a_small_thread_stack_gets_an_error_not_a_crash() {
    let out = std::thread::Builder::new()
        .stack_size(128 << 10)
        .spawn(|| {
            let mut iso =
                Runtime::new().new_isolate(Config::default().native_stack_limit(64 << 10));
            iso.scope(|s| {
                s.globals().set(
                    "callnow",
                    s.create_function(|c| {
                        let f: Function = c.check(0)?;
                        let r = f.call(c.args()[1..].to_vec())?;
                        c.ret(r)
                    }),
                )?;
                let src = "local n = 0
                     local function f() n = n + 1; return callnow(f) end
                     local ok, e = pcall(f)
                     return ok, e, n";
                let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
                out.iter()
                    .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                    .collect::<Result<Vec<_>, Error>>()
            })
            .unwrap()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(out[0], "false");
    assert!(out[1].contains("C stack overflow"), "{}", out[1]);
    eprintln!("levels before the budget ran out: {}", out[2]);
}

/// A loop that calls a native which calls back into script code uses no
/// more native stack on its last iteration than on its first. The plain
/// interpreter gives its handlers fuel so that a build whose handlers
/// call each other (a debug build) still returns to its trampoline, and
/// the callback's own run must not refill the fuel of the run around it.
#[test]
fn a_loop_of_callbacks_does_not_grow_the_native_stack() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    let count = iso
        .scope(|s| {
            let src = "local gsub = string.gsub
                 local n = 0
                 local function f(text)
                   local r = gsub(text, 'a', function() n = n + 1 end)
                   return r
                 end
                 for i = 1, 100000 do f('a') end
                 return n";
            let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
            Ok::<_, Error>(String::from_utf8_lossy(&out[0].to_display()?).into_owned())
        })
        .unwrap();
    assert_eq!(count, "100000");
}

/// Each kind of nested call, as a chunk that defines `f(d)`: `f` calls
/// `mark()` and then, while `d > 0`, calls itself one level deeper through
/// that kind of call. A row the interpreter runs without nesting on the
/// native stack measures 0, and its record keeps it that way.
const ROWS: &[(&str, &str)] = &[
    // A Rust native calling back with `Function::call`.
    (
        "host",
        "local function f(d) mark() if d > 0 then callnow(f, d - 1) end end
         return f",
    ),
    // A library function calling back: `string.gsub` with a function.
    (
        "gsub",
        "local function f(d)
           mark()
           if d > 0 then string.gsub('x', 'x', function() f(d - 1) end) end
         end
         return f",
    ),
    // `table.sort` with a comparison function.
    (
        "sort",
        "local function f(d)
           mark()
           if d > 0 then
             local once = true
             table.sort({2, 1}, function(a, b)
               if once then once = false f(d - 1) end
               return a < b
             end)
           end
         end
         return f",
    ),
    // An `__index` function.
    (
        "index",
        "local t
         t = setmetatable({}, {__index = function(_, d)
           mark()
           if d > 0 then local _ = t[d - 1] end
         end})
         return function(d) local _ = t[d] end",
    ),
    // `pcall`.
    (
        "pcall",
        "local function f(d) mark() if d > 0 then pcall(f, d - 1) end end
         return f",
    ),
    // A coroutine resumed from inside another.
    (
        "resume",
        "local function f(d) mark() if d > 0 then coroutine.wrap(f)(d - 1) end end
         return f",
    ),
];

/// Levels each row nests; the first two are left out of the measure, as
/// the first level starts from the host and not from a nested call.
const DEPTH: i64 = 12;

/// How far above its record a row may measure: a compiler update moves
/// frames by a few bytes, and a level is several frames.
fn allowed(recorded: usize) -> usize {
    recorded + (recorded / 10).max(64)
}

/// The native stack one more level of each row takes, in bytes.
fn measure() -> Vec<(&'static str, usize)> {
    let marks: Rc<RefCell<Vec<usize>>> = Rc::default();
    let mut iso = Runtime::new().new_isolate(Config::default());
    // The `sort` row needs the standard library's `table`.
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    iso.scope(|s| {
        let g = s.globals();
        let m = marks.clone();
        g.set(
            "mark",
            s.create_function(move |c| {
                let here = 0u8;
                m.borrow_mut()
                    .push(std::hint::black_box(&here) as *const u8 as usize);
                c.ret(())
            }),
        )?;
        g.set(
            "callnow",
            s.create_function(|c| {
                let f: Function = c.check(0)?;
                let r = f.call(c.args()[1..].to_vec())?;
                c.ret(r)
            }),
        )?;
        let mut rows = Vec::new();
        for &(name, src) in ROWS {
            let f = s.load(Chunk::new(src).name(name))?.call(())?[0]
                .as_function()
                .expect("each row returns its function");
            marks.borrow_mut().clear();
            f.call(DEPTH)?;
            let marks = marks.borrow();
            assert_eq!(marks.len() as i64, DEPTH + 1, "{name}");
            let level = marks[2..]
                .windows(2)
                .map(|w| w[0].abs_diff(w[1]))
                .max()
                .unwrap_or(0);
            rows.push((name, level));
        }
        Ok::<_, Error>(rows)
    })
    .unwrap()
}

fn expected_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/native-stack/expected.txt")
}

/// This platform's key in the record: frame sizes differ between targets.
fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Fails when a level of some row takes more native stack than its record
/// allows. With `UPDATE_EXPECTED=1` it records this run for this platform
/// instead. A platform with no record only prints its numbers.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "frame sizes are measured in release builds (cargo test --release)"
)]
fn bytes_per_level() {
    let rows = measure();
    let platform = platform();
    let path = expected_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut report = String::new();
    let mut failed = Vec::new();
    for &(name, bytes) in &rows {
        let key = format!("{platform} {name}");
        let recorded = text.lines().find_map(|l| {
            let (k, v) = l.rsplit_once(' ')?;
            (k == key).then(|| v.parse::<usize>().ok()).flatten()
        });
        let shown = recorded.map_or("-".into(), |r| r.to_string());
        writeln!(
            report,
            "{name:<8} {bytes:>6} bytes per level (record {shown})"
        )
        .unwrap();
        if recorded.is_some_and(|r| bytes > allowed(r)) {
            failed.push(name);
        }
    }
    eprintln!("native stack per nested level on {platform}:\n{report}");
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        let mut out: Vec<String> = text
            .lines()
            .filter(|l| !l.starts_with(&format!("{platform} ")))
            .map(str::to_owned)
            .collect();
        out.extend(rows.iter().map(|(n, b)| format!("{platform} {n} {b}")));
        std::fs::write(&path, out.join("\n") + "\n").unwrap();
        return;
    }
    assert!(
        failed.is_empty(),
        "a nested level takes more native stack than recorded in {}: {failed:?} \
         (shrink the frames, or record the new size with UPDATE_EXPECTED=1 and say why in the pull request)",
        path.display()
    );
}
