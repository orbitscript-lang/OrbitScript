//! Interrupting running Lua code from another thread (a CLI's Ctrl-C).

use std::thread;
use std::time::Duration;

use orbit::Lua;

/// Runs `src` while another thread requests an interrupt, and returns its
/// results as text.
fn interrupted(src: &str) -> Vec<String> {
    let mut lua = Lua::new();
    let handle = lua.vm().interrupt_handle();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        handle.request();
    });
    let out = lua.exec(src.as_bytes(), "t").unwrap();
    t.join().unwrap();
    let vm = lua.vm();
    out.iter()
        .map(|&v| String::from_utf8_lossy(&vm.to_display(v)).into_owned())
        .collect()
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
