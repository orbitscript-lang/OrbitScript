//! `os.setlocale`'s third argument (ADR-13): `"thread"` leaves a script's
//! locale on the thread after it returns, `"process"` changes the
//! process's. C code called through `ffi` shows what the thread uses. Its
//! own test program, since it changes the process's locale.

#![cfg(all(feature = "ffi", unix))]

use orbit::{Chunk, Config, Error, Isolate, Runtime};

fn isolate() -> Isolate {
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    orbit_std::install_ffi(&mut iso);
    iso
}

fn run(iso: &mut Isolate, src: &str) -> String {
    let src = format!(
        "local ffi = require 'std.ffi'
         local strtod = ffi.open():bind('strtod', 'double', 'cstr', 'ptr')
         local function comma() return strtod('1,5', nil) == 1.5 end
         {src}"
    );
    let out = iso.scope(|s| {
        let out = s.load(Chunk::new(&src).name("=t"))?.call(())?;
        out.iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, Error>>()
    });
    match out {
        Ok(v) => v.join("\t"),
        Err(e) => panic!("script failed: {e:?}"),
    }
}

#[test]
fn thread_and_process_scopes_outlive_the_run() {
    let mut a = isolate();
    // A C-locale isolate installs nothing, so it shows the thread's own.
    let mut c = isolate();
    if run(
        &mut a,
        "return os.setlocale('pt_BR.utf8', 'numeric') ~= nil",
    ) != "true"
    {
        eprintln!("pt_BR.utf8 is not installed; skipped");
        return;
    }
    assert_eq!(run(&mut a, "return comma()"), "true");
    assert_eq!(run(&mut c, "return comma()"), "false", "restored on return");

    // "thread": the thread keeps it after the run.
    assert_eq!(
        run(
            &mut a,
            "return os.setlocale('pt_BR.utf8', 'numeric', 'thread')"
        ),
        "pt_BR.utf8"
    );
    assert_eq!(run(&mut c, "return comma()"), "true");
    drop(a);
    assert_eq!(
        run(&mut c, "return comma()"),
        "true",
        "outlives the isolate"
    );
    let mut a = isolate();
    run(&mut a, "os.setlocale('C', 'numeric', 'thread')");
    assert_eq!(run(&mut c, "return comma()"), "false");

    // A locale left on the thread takes precedence over the process's
    // there, so the rest runs on a thread of its own.
    std::thread::spawn(process_scope).join().unwrap();
}

/// "process": the process's locale changes, as in PUC Lua, and the
/// isolate follows; the query asks the process.
fn process_scope() {
    let mut a = isolate();
    let mut c = isolate();
    assert_eq!(
        run(
            &mut a,
            "return os.setlocale('pt_BR.utf8', 'numeric', 'process'), tostring(1.5)"
        ),
        "pt_BR.utf8\t1,5"
    );
    assert_eq!(run(&mut c, "return comma(), tostring(1.5)"), "true\t1.5");
    assert_eq!(
        run(
            &mut c,
            "return os.setlocale(nil, 'numeric', 'process'), os.setlocale(nil, 'numeric')"
        ),
        "pt_BR.utf8\tC"
    );
    run(&mut a, "os.setlocale('C', 'all', 'process')");
    assert_eq!(run(&mut c, "return comma()"), "false");

    let bad = run(
        &mut a,
        "return select(2, pcall(os.setlocale, 'C', 'all', 'world'))",
    );
    assert!(
        bad.contains("bad argument #3") && bad.contains("(invalid option 'world')"),
        "{bad}"
    );
}
