//! Natives calling back into Lua and running the call to the end
//! (`NativeCtx::call`, what the C API's `lua_call` and `lua_pcall` use).

use orbit::Lua;
use orbit_vm::orbit_core::Value;
use orbit_vm::{LuaError, NativeCtx, NativeReturn};

/// `callnow(f, ...)`: calls `f(...)` now and returns its results.
fn callnow(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.nargs();
    let r = ctx.call(n - 1, None)?;
    Ok(NativeReturn::Return(r))
}

/// `pcallnow(h, f, ...)`: calls `f(...)` now with message handler `h`;
/// returns `true, results...` or `false, err`.
fn pcallnow(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.nargs();
    let at = ctx.len() - (n - 1);
    match ctx.call(n - 2, Some(0)) {
        Ok(r) => {
            ctx.insert(at, Value::Bool(true));
            Ok(NativeReturn::Return(r + 1))
        }
        Err(e) => {
            ctx.push(Value::Bool(false));
            ctx.push(e.value);
            Ok(NativeReturn::Return(2))
        }
    }
}

fn needs_table(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    ctx.check_table(0)?;
    Ok(NativeReturn::Return(0))
}

fn run(src: &str) -> Vec<String> {
    let mut lua = Lua::new();
    let vm = lua.vm();
    for (name, f) in [
        ("callnow", callnow as orbit_vm::NativeFn),
        ("pcallnow", pcallnow),
    ] {
        let f = vm.register_native(name, f);
        vm.set_global(name, f);
    }
    let out = lua.exec(src.as_bytes(), "t").unwrap();
    let vm = lua.vm();
    out.iter()
        .map(|&v| String::from_utf8_lossy(&vm.to_display(v)).into_owned())
        .collect()
}

#[test]
fn results_replace_the_function_and_arguments() {
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
fn errors_propagate_and_the_native_is_level_one() {
    // `error` at level 1 positions at its caller: the native, which has no
    // line, as in lua5.5.
    assert_eq!(run("return pcall(callnow, error, 'x')"), ["false", "x"]);
    assert_eq!(
        run("return pcall(callnow, function() error('y') end)"),
        ["false", "t:1: y"]
    );
}

#[test]
fn the_message_handler_runs_where_the_error_was_raised() {
    let out = run(
        "return pcallnow(function(m) return m .. '|' .. debug.traceback() end,
                     function() local x = nil; return x.y end)",
    );
    assert_eq!(out[0], "false");
    assert!(
        out[1].starts_with("t:2: attempt to index a nil value"),
        "{}",
        out[1]
    );
    assert!(out[1].contains("[C]: in global 'pcallnow'"), "{}", out[1]);
    assert!(out[1].contains("in function <t:2>"), "{}", out[1]);
    assert_eq!(
        run("return pcallnow(function(m) return 'handled: ' .. m end, error, 'z', 0)"),
        ["false", "handled: z"]
    );
    assert_eq!(
        run("return pcallnow(print, function(...) return ... end, 1, 2)"),
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
    let mut lua = Lua::new();
    let vm = lua.vm();
    let f = vm.register_native("needs_table", needs_table);
    vm.set_global("f", f);
    let out = lua
        .exec(
            b"package.loaded.mymod = {check = f}; f = nil
              return select(2, pcall(package.loaded.mymod.check, 1))",
            "t",
        )
        .unwrap();
    let vm = lua.vm();
    assert_eq!(
        String::from_utf8_lossy(&vm.to_display(out[0])),
        "bad argument #1 to 'mymod.check' (table expected, got number)"
    );
}
