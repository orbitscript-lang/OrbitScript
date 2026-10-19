//! The debugger engine (ADR-12) on compiled Lua and OrbitScript programs.
#![cfg(feature = "debugger")]

use std::cell::RefCell;
use std::rc::Rc;

use orbit_runtime::object::Value;
use orbit_runtime::vm::debugger::{
    Breakpoint, DebugEvents, ErrorBreak, Paused, Resume, StopReason,
};
use orbit_runtime::vm::{LuaError, NativeReturn, Vm};

fn compile(name: &str, src: &str) -> orbit_runtime::vm::op::UnlinkedChunk {
    let hir = if name.ends_with(".orb") {
        orbit_orb_syntax::to_hir(name, src.as_bytes(), &Default::default())
            .unwrap_or_else(|d| panic!("{d:?}"))
    } else {
        orbit_lua_syntax::to_hir(name, src.as_bytes(), &Default::default())
            .unwrap_or_else(|d| panic!("{d:?}"))
    };
    orbit_compiler::compile(&hir).expect("compiles")
}

fn run(vm: &mut Vm, name: &str, src: &str) -> Result<Vec<Value>, LuaError> {
    let main = vm.load(&compile(name, src));
    vm.call(main, &[])
}

/// A stop as the test sees it: why, the line, and the stopped function's
/// first local if it has one.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    why: &'static str,
    line: i64,
    local: Option<(String, i64)>,
}

type Script = Rc<RefCell<Vec<Resume>>>;
/// Each `breakpoints_resolved` call: the source and its breakpoints.
type Resolved = Rc<RefCell<Vec<(Vec<u8>, Vec<Breakpoint>)>>>;

struct Recorder {
    seen: Rc<RefCell<Vec<Seen>>>,
    /// Answers to give, in order; Continue once they run out.
    script: Script,
    resolved: Resolved,
}

impl DebugEvents for Recorder {
    fn stopped(&mut self, paused: &mut Paused<'_, '_>, reason: StopReason) -> Resume {
        let why = match reason {
            StopReason::Breakpoint { .. } => "breakpoint",
            StopReason::Step => "step",
            StopReason::Pause => "pause",
            StopReason::Error { caught: true, .. } => "caught error",
            StopReason::Error { caught: false, .. } => "error",
        };
        // The first script function: an error raised by `error` stops
        // with the native on top.
        let level = (0..)
            .map_while(|n| paused.level(n))
            .find(|&l| paused.ctx_ref().current_line(l) >= 0)
            .expect("a stopped script function");
        let ctx = paused.ctx();
        let line = ctx.current_line(level);
        let local = ctx.get_local(level, 1).map(|(name, v)| {
            let n = match v {
                Value::Int(i) => i,
                _ => -1,
            };
            (String::from_utf8(name).unwrap(), n)
        });
        self.seen.borrow_mut().push(Seen { why, line, local });
        let mut script = self.script.borrow_mut();
        if script.is_empty() {
            Resume::Continue
        } else {
            script.remove(0)
        }
    }

    fn breakpoints_resolved(&mut self, source: &[u8], breakpoints: &[Breakpoint]) {
        self.resolved
            .borrow_mut()
            .push((source.to_vec(), breakpoints.to_vec()));
    }
}

struct Session {
    seen: Rc<RefCell<Vec<Seen>>>,
    script: Script,
    resolved: Resolved,
}

fn attach(vm: &mut Vm, answers: &[Resume]) -> Session {
    let s = Session {
        seen: Default::default(),
        script: Rc::new(RefCell::new(answers.to_vec())),
        resolved: Default::default(),
    };
    vm.set_debug_events(Some(Box::new(Recorder {
        seen: s.seen.clone(),
        script: s.script.clone(),
        resolved: s.resolved.clone(),
    })));
    s
}

fn vm() -> Vm {
    let mut vm = Vm::new();
    orbit_runtime::stdlib::open_libs(&mut vm);
    vm
}

fn lines(s: &Session) -> Vec<(&'static str, i64)> {
    s.seen.borrow().iter().map(|x| (x.why, x.line)).collect()
}

const PROGRAM: &str = "\
local function add(a, b)
  local s = a + b
  return s
end

local total = 0
for i = 1, 3 do
  total = add(total, i)
end
return total
";

/// Load `src` once without running it, so its functions exist for
/// breakpoints to resolve against, and return the main function.
fn loaded(vm: &mut Vm, name: &str, src: &str) -> Value {
    vm.load(&compile(name, src))
}

#[test]
fn breakpoint_stops_each_time_its_line_runs() {
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "t.lua", PROGRAM);
    let bps = vm.set_breakpoints(b"@t.lua", &[2]);
    assert_eq!(
        bps,
        [Breakpoint {
            requested: 2,
            line: Some(2)
        }]
    );
    let r = vm.call(main, &[]).unwrap();
    assert!(matches!(r[..], [Value::Int(6)]));
    assert_eq!(lines(&s), [("breakpoint", 2); 3]);
    // The stopped function's locals: `a` is its first.
    let seen = s.seen.borrow();
    assert_eq!(seen[2].local, Some(("a".into(), 3)));
}

#[test]
fn breakpoint_on_a_line_without_code_moves_to_the_next_one() {
    let mut vm = vm();
    let _s = attach(&mut vm, &[]);
    let _main = loaded(&mut vm, "t.lua", PROGRAM);
    let bps = vm.set_breakpoints(b"@t.lua", &[5, 4, 100]);
    let got: Vec<_> = bps.iter().map(|b| b.line).collect();
    // Line 4 (`end`) has the function's return; line 5 is blank.
    assert_eq!(got, [Some(6), Some(4), None]);
}

#[test]
fn breakpoints_set_before_loading_resolve_when_the_chunk_links() {
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let bps = vm.set_breakpoints(b"@t.lua", &[8]);
    assert_eq!(bps[0].line, None);
    run(&mut vm, "t.lua", PROGRAM).unwrap();
    assert_eq!(lines(&s), [("breakpoint", 8); 3]);
    let resolved = s.resolved.borrow();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, b"@t.lua");
    assert_eq!(resolved[0].1[0].line, Some(8));
}

#[test]
fn clearing_breakpoints_stops_no_more() {
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "t.lua", PROGRAM);
    vm.set_breakpoints(b"@t.lua", &[2]);
    vm.set_breakpoints(b"@t.lua", &[]);
    vm.call(main, &[]).unwrap();
    assert!(lines(&s).is_empty());
}

#[test]
fn step_over_runs_calls_and_stops_on_each_line() {
    let mut vm = vm();
    let s = attach(&mut vm, &[Resume::StepOver; 6]);
    let main = loaded(&mut vm, "t.lua", PROGRAM);
    vm.set_breakpoints(b"@t.lua", &[6]);
    vm.call(main, &[]).unwrap();
    assert_eq!(
        lines(&s),
        [
            ("breakpoint", 6),
            ("step", 7),
            ("step", 8),
            ("step", 7),
            ("step", 8),
            ("step", 7),
            ("step", 8),
        ]
    );
}

#[test]
fn step_in_enters_the_callee_and_step_out_leaves_it() {
    let mut vm = vm();
    let s = attach(
        &mut vm,
        &[
            Resume::StepIn,
            Resume::StepIn,
            Resume::StepOut,
            Resume::StepOver,
        ],
    );
    let main = loaded(&mut vm, "t.lua", PROGRAM);
    vm.set_breakpoints(b"@t.lua", &[8]);
    vm.call(main, &[]).unwrap();
    let got = lines(&s);
    // In at line 2, the next line 3, then out: back in the loop's body at
    // the call, then on to the loop's next iteration.
    assert_eq!(
        got[..5],
        [
            ("breakpoint", 8),
            ("step", 2),
            ("step", 3),
            ("step", 8),
            ("step", 7),
        ]
    );
}

#[test]
fn step_over_a_recursive_call_stops_after_it() {
    let src = "\
local function f(n)
  if n == 0 then return 0 end
  local r = f(n - 1)
  return r + 1
end
return f(3)
";
    let mut vm = vm();
    let s = attach(&mut vm, &[Resume::StepOver, Resume::StepOver]);
    let main = loaded(&mut vm, "r.lua", src);
    vm.set_breakpoints(b"@r.lua", &[3]);
    vm.call(main, &[]).unwrap();
    let got = lines(&s);
    // Stopped at line 3 in the outermost call, stepped over the recursion
    // (whose own line 3 breakpoints still stop), then line 4.
    assert_eq!(got[0], ("breakpoint", 3));
    assert_eq!(got[1], ("breakpoint", 3));
}

#[test]
fn step_over_comes_back_through_recursion() {
    let src = "\
local function g() return 1 end
local function f(n)
  if n > 0 then f(n - 1) end
  g()
  return n
end
f(1)
";
    let mut vm = vm();
    let s = attach(&mut vm, &[Resume::StepIn, Resume::StepOver]);
    let main = loaded(&mut vm, "r.lua", src);
    vm.set_breakpoints(b"@r.lua", &[7]);
    vm.call(main, &[]).unwrap();
    // The inner `f(0)` calls `g`, which runs plain, and returns into
    // `f(1)` without changing code: the step still stops there.
    let seen = s.seen.borrow();
    let got: Vec<_> = seen.iter().map(|x| (x.why, x.line)).collect();
    assert_eq!(got, [("breakpoint", 7), ("step", 3), ("step", 4)]);
    assert_eq!(seen[2].local, Some(("n".into(), 1)));
}

#[test]
fn pause_stops_a_running_loop() {
    let src = "\
local n = 0
while n < 100000 do
  n = n + 1
end
return n
";
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "p.lua", src);
    vm.pause_handle().request();
    let r = vm.call(main, &[]).unwrap();
    assert!(matches!(r[..], [Value::Int(100000)]));
    let got = lines(&s);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "pause");
}

#[test]
fn pause_from_another_thread() {
    let src = "\
local n = 0
while n < 50000000 do
  n = n + 1
end
return n
";
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "p.lua", src);
    let pause = vm.pause_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(5));
        pause.request();
    });
    vm.call(main, &[]).unwrap();
    t.join().unwrap();
    assert_eq!(lines(&s).len(), 1);
}

#[test]
fn errors_stop_before_unwinding() {
    let src = "\
local function boom() error('no') end
pcall(boom)
local x = 1
boom()
";
    for (on, want) in [
        (ErrorBreak::Never, vec![]),
        (ErrorBreak::Uncaught, vec![("error", 1)]),
        (ErrorBreak::All, vec![("caught error", 1), ("error", 1)]),
    ] {
        let mut vm = vm();
        let s = attach(&mut vm, &[]);
        vm.set_error_break(on);
        // Both errors come from `boom`, on line 1.
        assert!(run(&mut vm, "e.lua", src).is_err());
        assert_eq!(lines(&s), want, "{on:?}");
    }
}

#[test]
fn errors_in_coroutines_count_as_caught() {
    let src = "\
local co = coroutine.create(function() error('x') end)
coroutine.resume(co)
";
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    vm.set_error_break(ErrorBreak::Uncaught);
    run(&mut vm, "c.lua", src).unwrap();
    assert!(lines(&s).is_empty());
}

#[test]
fn breakpoints_stop_inside_coroutines() {
    let src = "\
local co = coroutine.wrap(function()
  for i = 1, 2 do
    coroutine.yield(i)
  end
end)
co()
co()
";
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "c.lua", src);
    vm.set_breakpoints(b"@c.lua", &[3]);
    vm.call(main, &[]).unwrap();
    assert_eq!(lines(&s), [("breakpoint", 3); 2]);
}

#[test]
fn step_in_through_a_builtin_stops_in_the_callback() {
    let src = "\
local t = {3, 1, 2}
table.sort(t, function(a, b)
  return a < b
end)
return t[1]
";
    let mut vm = vm();
    let s = attach(&mut vm, &[Resume::StepIn; 3]);
    let main = loaded(&mut vm, "s.lua", src);
    vm.set_breakpoints(b"@s.lua", &[2]);
    vm.call(main, &[]).unwrap();
    // The comparator is made on line 4 and the call is on line 2.
    // `table.sort` is written in OrbitScript but runs like C: stepping in
    // stops at the comparator, never inside the sort.
    assert_eq!(
        lines(&s)[..4],
        [("breakpoint", 2), ("step", 4), ("step", 2), ("step", 3)]
    );
}

#[test]
fn a_lua_hook_and_the_debugger_work_together() {
    let src = "\
local lines = 0
debug.sethook(function() lines = lines + 1 end, 'l')
local x = 1
x = x + 1
debug.sethook()
return lines
";
    let mut vm = vm();
    let s = attach(&mut vm, &[]);
    let main = loaded(&mut vm, "h.lua", src);
    vm.set_breakpoints(b"@h.lua", &[4]);
    let r = vm.call(main, &[]).unwrap();
    assert_eq!(lines(&s), [("breakpoint", 4)]);
    // Lines 3, 4 and 5: the stop doesn't run the hook.
    assert!(matches!(r[..], [Value::Int(3)]), "{r:?}");
}

#[test]
fn nothing_stops_without_events() {
    let mut vm = vm();
    let main = loaded(&mut vm, "t.lua", PROGRAM);
    vm.set_breakpoints(b"@t.lua", &[2]);
    vm.set_error_break(ErrorBreak::All);
    vm.call(main, &[]).unwrap();
}

#[test]
fn orbitscript_breakpoints_and_steps() {
    let src = "\
fn add(a, b) {
    let s = a + b;
    s
}

let mut total = 0;
for i in 1..=3 {
    total = add(total, i);
}
return total;
";
    let mut vm = vm();
    let s = attach(&mut vm, &[Resume::StepIn]);
    let main = loaded(&mut vm, "t.orb", src);
    vm.set_breakpoints(b"@t.orb", &[8]);
    vm.call(main, &[]).unwrap();
    let got = lines(&s);
    assert_eq!(got[..2], [("breakpoint", 8), ("step", 2)]);
}

/// What an attached debugger costs code it can't stop in: each benchmark
/// with no debugger, then with one attached and a breakpoint in a function
/// the benchmark never calls. Run with
/// `cargo test --release --features debugger --test debugger -- --ignored --nocapture`.
#[test]
#[ignore]
fn overhead_outside_breakpoints() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench");
    // The manifest's full inputs.
    let benches = [
        ("fib.lua", "35"),
        ("binary_trees.lua", "16"),
        ("nbody.lua", "1000000"),
        ("spectral_norm.lua", "500"),
        ("fannkuch.lua", "10"),
        ("strings.lua", "500000"),
        ("tables.lua", "1000000"),
        ("oop.lua", "2000000"),
        ("coroutines.lua", "4000000"),
    ];
    let runs = 5;
    for (name, size) in benches {
        let src = std::fs::read_to_string(dir.join(name)).unwrap();
        let chunk = compile(name, &src);
        let mut times = [Vec::new(), Vec::new()];
        for _ in 0..runs {
            for (mode, t) in times.iter_mut().enumerate() {
                let mut vm = vm();
                // `print` lives in `orbit-std`; the checksums aren't needed.
                let print = vm.register_native("print", |_| Ok(NativeReturn::Return(0)));
                vm.set_global("print", print);
                let arg = vm.new_table();
                let size = vm.new_str(size.as_bytes());
                vm.table_set(arg, Value::Int(1), size);
                vm.set_global("arg", Value::Table(arg));
                let _s = (mode == 1).then(|| {
                    let s = attach(&mut vm, &[]);
                    let other = loaded(
                        &mut vm,
                        "other.lua",
                        "local function f()\n  return 1\nend\n",
                    );
                    vm.set_global("keep", other);
                    assert_eq!(vm.set_breakpoints(b"@other.lua", &[2])[0].line, Some(2));
                    s
                });
                let main = vm.load(&chunk);
                let start = std::time::Instant::now();
                if let Err(e) = vm.call(main, &[]) {
                    panic!(
                        "{name}: {}",
                        String::from_utf8_lossy(&vm.to_display(e.value))
                    );
                }
                t.push(start.elapsed().as_secs_f64());
            }
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (a, b) = (med(&mut times[0]), med(&mut times[1]));
        eprintln!(
            "{name:18} none {a:.3}s  attached {b:.3}s  ratio {:.3}",
            b / a
        );
    }
}
