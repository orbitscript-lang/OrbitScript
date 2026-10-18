//! Interrupting running Lua code from another thread (a CLI's Ctrl-C).

use std::thread;
use std::time::Duration;

use orbit::{Chunk, Config, Error, Runtime};

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
        "for i = 1, math.maxinteger do end",
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
