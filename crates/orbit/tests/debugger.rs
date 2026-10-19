//! The debugger through the embedding API (ADR-12).
#![cfg(feature = "debugger")]

use std::cell::RefCell;
use std::rc::Rc;

use orbit::debugger::{Breakpoint, DebugEvents, ErrorBreak, Paused, Resume, StopReason};
use orbit::{Chunk, Config, Debugger, Isolate, Language, Runtime, Value};

/// What a stop looked like.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    why: StopReason,
    /// `name@line` for each activation, innermost first.
    frames: Vec<String>,
    /// `name=value` for each visible local of level 0.
    locals: Vec<String>,
}

type Log = Rc<RefCell<Vec<Seen>>>;
/// Runs on each stop before it is recorded.
type Edit = Box<dyn FnMut(&mut Paused<'_, '_>)>;
/// Each `breakpoints_resolved` call.
type Resolved = Rc<RefCell<Vec<(Vec<u8>, Vec<Breakpoint>)>>>;

/// Records each stop, then answers from `answers` (`Continue` when they
/// run out); `edit` runs on each stop first.
struct Recorder {
    log: Log,
    answers: Vec<Resume>,
    edit: Option<Edit>,
    resolved: Resolved,
}

fn show(p: &mut Paused<'_, '_>, v: &orbit::Pinned<Value<'static>>) -> String {
    p.scope(|s| {
        let v = v.get(s).unwrap();
        String::from_utf8_lossy(&v.to_display().unwrap()).into_owned()
    })
}

impl DebugEvents for Recorder {
    fn stopped(&mut self, p: &mut Paused<'_, '_>, why: StopReason) -> Resume {
        if let Some(edit) = &mut self.edit {
            edit(p);
        }
        let frames = p
            .frames()
            .iter()
            .map(|f| {
                let name = f
                    .name
                    .as_deref()
                    .map(String::from_utf8_lossy)
                    .unwrap_or_default();
                format!("{name}@{}", f.currentline)
            })
            .collect();
        let locals = p
            .locals(0)
            .iter()
            .map(|v| format!("{}={}", String::from_utf8_lossy(&v.name), show(p, &v.value)))
            .collect();
        self.log.borrow_mut().push(Seen {
            why,
            frames,
            locals,
        });
        if self.answers.is_empty() {
            Resume::Continue
        } else {
            self.answers.remove(0)
        }
    }

    fn breakpoints_resolved(&mut self, source: &[u8], bps: &[Breakpoint]) {
        self.resolved
            .borrow_mut()
            .push((source.to_vec(), bps.to_vec()));
    }
}

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

fn attach(iso: &mut Isolate, answers: &[Resume]) -> Log {
    let log = Log::default();
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Rc::clone(&log),
        answers: answers.to_vec(),
        edit: None,
        resolved: Rc::default(),
    })))
    .unwrap();
    log
}

const PROGRAM: &str = "\
local function add(a, b)
  local sum = a + b
  return sum
end
local x = add(1, 2)
local y = add(x, 10)
return y
";

fn run(iso: &mut Isolate, src: &str) -> Result<Vec<String>, String> {
    iso.scope(|s| {
        let r = s.load(Chunk::new(src).name("=prog"))?.call(())?;
        r.iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, orbit::Error>>()
    })
    .map_err(|e| e.to_string())
}

#[test]
fn breakpoints_show_frames_and_locals() {
    let mut iso = isolate();
    let log = attach(&mut iso, &[]);
    let bps = iso.set_breakpoints(b"=prog", &[3]);
    assert_eq!(
        bps,
        [Breakpoint {
            requested: 3,
            line: None
        }]
    );
    assert_eq!(run(&mut iso, PROGRAM).unwrap(), ["13"]);
    let log = log.borrow();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].why, StopReason::Breakpoint { line: 3 });
    assert_eq!(log[0].frames, ["add@3", "@5"]);
    assert_eq!(log[0].locals, ["a=1", "b=2", "sum=3"]);
    assert_eq!(log[1].locals, ["a=3", "b=10", "sum=13"]);
}

#[test]
fn breakpoints_resolve_on_load() {
    let mut iso = isolate();
    let resolved = Rc::default();
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Log::default(),
        answers: Vec::new(),
        edit: None,
        resolved: Rc::clone(&resolved),
    })))
    .unwrap();
    iso.set_breakpoints(b"=prog", &[4, 100]);
    run(&mut iso, PROGRAM).unwrap();
    let resolved = resolved.borrow();
    assert_eq!(
        resolved[0],
        (
            b"=prog".to_vec(),
            vec![
                Breakpoint {
                    requested: 4,
                    line: Some(4)
                },
                Breakpoint {
                    requested: 100,
                    line: None
                },
            ]
        )
    );
    // Once loaded, setting them answers at once.
    let bps = iso.set_breakpoints(b"=prog", &[6]);
    assert_eq!(
        bps,
        [Breakpoint {
            requested: 6,
            line: Some(6)
        }]
    );
}

#[test]
fn stepping() {
    let mut iso = isolate();
    let log = attach(
        &mut iso,
        &[
            Resume::StepIn,
            Resume::StepOver,
            Resume::StepOut,
            Resume::StepOver,
        ],
    );
    iso.set_breakpoints(b"=prog", &[5]);
    run(&mut iso, PROGRAM).unwrap();
    let lines: Vec<String> = log.borrow().iter().map(|s| s.frames[0].clone()).collect();
    // Stepping out stops at the caller's next line.
    assert_eq!(lines, ["@5", "add@2", "add@3", "@6", "@7"]);
}

#[test]
fn set_local_changes_the_program() {
    let mut iso = isolate();
    let log = Log::default();
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Rc::clone(&log),
        answers: Vec::new(),
        edit: Some(Box::new(|p| {
            let sum = p.locals(0).into_iter().find(|v| v.name == b"sum").unwrap();
            p.set_local(0, sum.index, |s| s.value(40)).unwrap();
            assert!(p.set_local(0, 99, |s| s.value(40)).is_err());
        })),
        resolved: Rc::default(),
    })))
    .unwrap();
    iso.set_breakpoints(b"=prog", &[3]);
    // add returns 40 both times: y = 40.
    assert_eq!(run(&mut iso, PROGRAM).unwrap(), ["40"]);
    // The edit runs before the stop is logged.
    assert_eq!(log.borrow()[1].locals, ["a=40", "b=10", "sum=40"]);
}

#[test]
fn upvalues_and_globals() {
    let mut iso = isolate();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let out = Rc::clone(&seen);
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Log::default(),
        answers: Vec::new(),
        edit: Some(Box::new(move |p| {
            for v in p.upvalues(0) {
                let text = show(p, &v.value);
                out.borrow_mut()
                    .push(format!("{}={text}", String::from_utf8_lossy(&v.name)));
            }
            let g = p.globals();
            let answer = p.scope(|s| g.get(s).unwrap().get::<i64>("answer").unwrap());
            out.borrow_mut().push(format!("answer={answer}"));
        })),
        resolved: Rc::default(),
    })))
    .unwrap();
    iso.set_breakpoints(b"=prog", &[4]);
    run(
        &mut iso,
        "answer = 42\nlocal n = 7\nlocal function f()\n  return n\nend\nreturn f()",
    )
    .unwrap();
    assert_eq!(*seen.borrow(), ["n=7", "answer=42"]);
}

#[test]
fn errors_stop_with_their_value() {
    let mut iso = isolate();
    let values = Rc::new(RefCell::new(Vec::new()));
    let out = Rc::clone(&values);
    let log = Log::default();
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Rc::clone(&log),
        answers: Vec::new(),
        edit: Some(Box::new(move |p| {
            if let Some(e) = p.error() {
                let text = show(p, &e);
                out.borrow_mut().push(text);
            }
        })),
        resolved: Rc::default(),
    })))
    .unwrap();
    iso.set_error_break(ErrorBreak::All);
    let r = run(
        &mut iso,
        "pcall(error, 'caught')\nlocal t = nil\nreturn t.x",
    );
    assert!(r.unwrap_err().contains("attempt to index"));
    let log = log.borrow();
    let whys: Vec<_> = log.iter().map(|s| s.why).collect();
    assert_eq!(
        whys,
        [
            StopReason::Error { caught: true },
            StopReason::Error { caught: false }
        ]
    );
    assert_eq!(values.borrow()[0], "caught");
    assert!(
        values.borrow()[1].contains("attempt to index a nil value"),
        "{:?}",
        values.borrow()
    );
}

#[test]
fn pause_from_another_thread() {
    let mut iso = isolate();
    let log = Log::default();
    iso.set_debug_events(Some(Box::new(Recorder {
        log: Rc::clone(&log),
        answers: Vec::new(),
        // Let the loop end.
        edit: Some(Box::new(|p| {
            let g = p.globals();
            p.scope(|s| g.get(s).unwrap().set("stop", true).unwrap());
        })),
        resolved: Rc::default(),
    })))
    .unwrap();
    let pause = iso.pause_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        pause.request();
    });
    run(
        &mut iso,
        "local n = 0\nwhile not stop do\n  n = n + 1\nend\nreturn n",
    )
    .unwrap();
    t.join().unwrap();
    let log = log.borrow();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].why, StopReason::Pause);
    assert!(
        ["@2", "@3"].contains(&&*log[0].frames[0]),
        "{:?}",
        log[0].frames
    );
}

#[test]
fn disabled_isolates_refuse_a_debugger() {
    let mut iso = Runtime::new().new_isolate(Config::default().debugger(Debugger::Disabled));
    let e = iso.set_debug_events(None).unwrap_err();
    assert!(e.to_string().contains("allows no debugger"));
    assert_eq!(
        iso.set_breakpoints(b"=prog", &[1]),
        [Breakpoint {
            requested: 1,
            line: None
        }]
    );
    // Pausing does nothing, and no error break interrupts.
    iso.pause_handle().request();
    iso.set_error_break(ErrorBreak::All);
    assert_eq!(run(&mut iso, PROGRAM).unwrap(), ["13"]);
}

#[test]
fn orbitscript_frames() {
    let mut iso = isolate();
    let log = attach(&mut iso, &[]);
    iso.set_breakpoints(b"=prog.orb", &[2]);
    let src = "fn twice(n) {\n    let m = n * 2;\n    m\n}\ntwice(21)";
    iso.scope(|s| {
        s.load(
            Chunk::new(src)
                .name("=prog.orb")
                .language(Language::OrbitScript),
        )?
        .call(())
        .map(drop)
    })
    .unwrap();
    let log = log.borrow();
    assert_eq!(log.len(), 1);
    // The OrbitScript front end doesn't name item calls yet.
    assert!(log[0].frames[0].ends_with("@2"), "{:?}", log[0].frames);
    assert_eq!(log[0].locals, ["n=21"]);
}
