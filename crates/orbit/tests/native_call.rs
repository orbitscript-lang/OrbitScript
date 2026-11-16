//! Host functions calling back into scripts and running the call to the end
//! ([`Function::call`], built on what the C API's `lua_call` uses).

use orbit::{Chunk, Config, Error, Function, Isolate, Runtime, Value};

fn isolate() -> Isolate {
    Runtime::new().new_isolate(Config::default())
}

/// Runs `src` named `t` with the host functions `callnow` and `tracenow`,
/// and returns its results as `tostring` shows them.
fn run(src: &str) -> Vec<String> {
    let mut iso = isolate();
    iso.scope(|s| {
        let g = s.globals();
        // `callnow(f, ...)`: calls `f(...)` now and returns its results.
        g.set(
            "callnow",
            s.create_function(|c| {
                let f: Function = c.check(0)?;
                let r = f.call(c.args()[1..].to_vec())?;
                c.ret(r)
            }),
        )?;
        // `tracenow(f, ...)`: `true, results...`, or `false, message,
        // traceback` from where the error was raised.
        g.set(
            "tracenow",
            s.create_function(|c| {
                let f: Function = c.check(0)?;
                match f.call_traced(c.args()[1..].to_vec()) {
                    Ok(mut r) => {
                        r.insert(0, Value::Bool(true));
                        c.ret(r)
                    }
                    Err(e) => c.ret((
                        false,
                        e.message().to_owned(),
                        e.traceback().unwrap_or("").to_owned(),
                    )),
                }
            }),
        )?;
        let out = s.load(Chunk::new(src).name("=t"))?.call(())?;
        out.iter()
            .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
            .collect::<Result<Vec<_>, Error>>()
    })
    .unwrap()
}

#[test]
fn results_come_back_in_order() {
    assert_eq!(
        run("return 'below', callnow(function(a, b) return b, a, a + b end, 1, 2)"),
        ["below", "2", "1", "3"]
    );
    assert_eq!(
        run("return callnow(callnow, callnow, select, '#', nil, nil)"),
        ["2"]
    );
}

#[test]
fn errors_propagate_and_the_host_function_is_level_one() {
    // `error` at level 1 positions at its caller: the host function, which
    // has no line, as in lua5.5.
    assert_eq!(run("return pcall(callnow, error, 'x')"), ["false", "x"]);
    assert_eq!(
        run("return pcall(callnow, function() error('y') end)"),
        ["false", "t:1: y"]
    );
}

#[test]
fn tracebacks_show_where_the_error_was_raised() {
    let out = run("return tracenow(function()
                     local x = nil; return x.y end)");
    assert_eq!(out[0], "false");
    assert!(
        out[1].starts_with("t:2: attempt to index a nil value"),
        "{}",
        out[1]
    );
    assert!(out[2].starts_with("stack traceback:"), "{}", out[2]);
    assert!(out[2].contains("in function <t:1>"), "{}", out[2]);
    assert!(out[2].contains("[C]: in global 'tracenow'"), "{}", out[2]);
    assert_eq!(
        run("return tracenow(function(...) return ... end, 1, 2)"),
        ["true", "1", "2"]
    );
}

#[test]
fn nothing_yields_across_it() {
    assert_eq!(
        run(
            "local co = coroutine.wrap(function() callnow(coroutine.yield, 1) end)
             return pcall(co)"
        ),
        ["false", "attempt to yield across a C-call boundary"]
    );
}

#[test]
fn deep_recursion_through_it_overflows_cleanly() {
    // Each level nests the interpreter on the native stack; unoptimized
    // builds need more than a test thread's default.
    let stack = std::env::var("NATIVE_CALL_STACK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64 << 20);
    let out = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(|| run("local function f() return callnow(f) end return pcall(f)"))
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(out[0], "false");
    assert!(out[1].contains("stack overflow"), "{}", out[1]);
}

#[test]
fn argument_errors_name_the_function_by_its_module() {
    let mut iso = isolate();
    let msg: String = iso
        .scope(|s| {
            let f = s.create_function(|c| {
                c.check::<orbit::Table>(0)?;
                c.ret(())
            });
            s.globals().set("f", f)?;
            // `package.loaded`, which orbit-std's `package` library shows.
            s.globals()
                .set("loaded", s.registry().get::<Value>("_LOADED")?)?;
            let r = s
                .load(Chunk::new(
                    "loaded.mymod = {check = f}; f = nil
                     return select(2, pcall(loaded.mymod.check, 1))",
                ))?
                .call(())?;
            r[0].get()
        })
        .unwrap();
    assert_eq!(
        msg,
        "bad argument #1 to 'mymod.check' (table expected, got number)"
    );
}

#[test]
fn host_functions_show_scripts_no_upvalues() {
    // A host function's state lives in a hidden upvalue the debug library
    // can't see or change.
    let mut iso = isolate();
    let out: Vec<String> = iso
        .scope(|s| {
            let f = s.create_function(|c| c.ret(7));
            s.globals().set("f", f)?;
            let r = s
                .load(Chunk::new(
                    "return debug.getupvalue(f, 1), debug.setupvalue(f, 1, 99),
                            debug.getinfo(f, 'u').nups, f()",
                ))?
                .call(())?;
            r.iter()
                .map(|v| Ok(String::from_utf8_lossy(&v.to_display()?).into_owned()))
                .collect::<Result<Vec<_>, Error>>()
        })
        .unwrap();
    assert_eq!(out, ["nil", "nil", "0", "7"]);
}

/// The traceback of an error that leaves through to-be-closed variables is
/// the stack as the error was raised, not what is left once their
/// `__close` handlers have run; and a traced call that fails inside one of
/// those handlers reports its own stack without taking the outer error's.
#[test]
fn tracebacks_are_taken_before_variables_close() {
    const TBC: &str = "local function tbc(f) return setmetatable({}, {__close = f}) end\n";
    let out = run(&format!(
        "{TBC}local function raiser()
           local v <close> = tbc(function() end)
           local x = nil; return x.y
         end
         return tracenow(function() raiser() end)"
    ));
    assert_eq!(out[0], "false");
    assert!(out[1].starts_with("t:4: attempt to index"), "{}", out[1]);
    assert!(out[2].contains("t:4: in upvalue 'raiser'"), "{}", out[2]);

    let out = run(&format!(
        "{TBC}local inner
         local function nested() local x = nil; return x.z end
         local function raiser()
           local v <close> = tbc(function() inner = {{tracenow(nested)}} end)
           local w <close> = tbc(function() pcall(callnow, error, 'ignored') end)
           local x = nil; return x.y
         end
         local ok, msg, trace = tracenow(function() raiser() end)
         return ok, msg, trace, inner[2], inner[3]"
    ));
    assert_eq!(out[0], "false");
    assert!(out[1].starts_with("t:7: attempt to index"), "{}", out[1]);
    assert!(out[2].contains("t:7: in upvalue 'raiser'"), "{}", out[2]);
    assert!(!out[2].contains("'nested'"), "{}", out[2]);
    assert!(out[3].starts_with("t:3: attempt to index"), "{}", out[3]);
    assert!(out[4].contains("t:3: in function <t:3>"), "{}", out[4]);
    assert!(!out[4].contains("'raiser'"), "{}", out[4]);
}
