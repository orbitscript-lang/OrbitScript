//! Interrupting running Lua code from another thread (a CLI's Ctrl-C).

use std::thread;
use std::time::Duration;

use orbit::{Chunk, Config, Error, ErrorKind, Runtime};

/// Runs `src` while another thread requests an interrupt, and returns its
/// results as text.
fn interrupted(src: &str) -> Vec<String> {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let handle = iso.interrupt();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        assert!(!handle.pending());
        handle.request();
        assert!(handle.pending());
    });
    let out = iso
        .scope(|s| {
            let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
            out.iter()
                .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, Error>>()
        })
        .unwrap();
    t.join().unwrap();
    out
}

#[test]
fn interrupt_stops_endless_loops() {
    // The error names the interrupted function's caller, like the
    // reference's; the main chunk's caller is the host.
    for body in [
        "while true do end",
        "repeat local x = 1 until false",
        "for i = 1, 0x7fffffffffffffff do end",
        "for k in function() return 1 end do end",
    ] {
        let src = format!(
            "local function f() {body} end
             local ok, msg = pcall(function()
               f()
             end)
             local again = 0
             for i = 1, 1000 do again = again + 1 end
             return ok, msg, again"
        );
        assert_eq!(
            interrupted(&src),
            ["false", "t:3: interrupted!", "1000"],
            "{body}"
        );
    }
    assert_eq!(
        interrupted("return pcall(function() while true do end end)"),
        ["false", "interrupted!"]
    );
}

#[test]
fn interrupt_reaches_hooked_code() {
    let src = "local n = 0
               debug.sethook(function() n = n + 1 end, 'l')
               local ok, msg = pcall(function() while true do end end)
               debug.sethook()
               return ok, msg, n > 0";
    assert_eq!(interrupted(src), ["false", "interrupted!", "true"]);
}

/// The kind of error `src` ends in, interrupted by another thread when
/// `interrupt` is set.
fn kind_of(src: &str, interrupt: bool) -> ErrorKind {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let handle = iso.interrupt();
    let t = thread::spawn(move || {
        if interrupt {
            thread::sleep(Duration::from_millis(50));
            handle.request();
        }
    });
    let err = iso
        .scope(|s| s.load(Chunk::new(src).name("=t"))?.call(()).map(|_| ()))
        .unwrap_err();
    t.join().unwrap();
    err.kind()
}

#[test]
fn only_a_real_interrupt_is_reported_as_one() {
    assert_eq!(kind_of("while true do end", true), ErrorKind::Interrupted);
    assert_eq!(
        kind_of("coroutine.wrap(function() while true do end end)()", true),
        ErrorKind::Interrupted
    );
    // A `__close` handler that returns lets the interrupt go on as one.
    assert_eq!(
        kind_of(
            "local x <close> = setmetatable({}, {__close = function() end})
             while true do end",
            true
        ),
        ErrorKind::Interrupted
    );
    // A `__close` handler's error replaces the interrupt.
    assert_eq!(
        kind_of(
            "coroutine.wrap(function()
               local x <close> = setmetatable({}, {__close = function() error('closing') end})
               while true do end
             end)()",
            true
        ),
        ErrorKind::Runtime
    );
    // A script raising the same text is an ordinary error, whatever it says.
    for src in [
        "error('interrupted!')",
        "error('interrupted!', 0)",
        "error(setmetatable({}, {__tostring = function() return 'interrupted!' end}))",
    ] {
        assert_eq!(kind_of(src, false), ErrorKind::Runtime, "{src}");
    }
}

#[test]
fn an_interrupted_coroutine_reports_the_interrupt_to_its_host() {
    let mut iso = Runtime::new().new_isolate(Config::default());
    let handle = iso.interrupt();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        handle.request();
    });
    let kinds = iso
        .scope(|s| {
            let f = s.load(Chunk::new("while true do end").name("=t"))?;
            let co = s.create_thread(f);
            let first = co.resume(()).map(|_| ()).unwrap_err().kind();
            // Resuming it again fails for being dead, not for the interrupt.
            let again = co.resume(()).map(|_| ()).unwrap_err().kind();
            Ok::<_, Error>((first, again))
        })
        .unwrap();
    t.join().unwrap();
    assert_eq!(kinds, (ErrorKind::Interrupted, ErrorKind::Runtime));
}
