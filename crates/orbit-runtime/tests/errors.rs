//! What an error carries out of the VM when no message handler is set:
//! the traceback of where it was raised.

use orbit_runtime::object::Value;
use orbit_runtime::vm::{LuaError, NativeCtx, NativeReturn, Vm};

/// `callnow(f, ...)`: calls `f(...)` to the end before returning, the way a
/// host function does, and returns its results.
fn callnow(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.call(ctx.len() - 1, None)?;
    Ok(NativeReturn::Return(n))
}

fn run(src: &str) -> Result<Vec<Value>, LuaError> {
    let mut vm = Vm::new();
    orbit_runtime::stdlib::open_libs(&mut vm);
    let f = vm.register_native("callnow", callnow);
    vm.set_global("callnow", f);
    let hir = orbit_lua_syntax::to_hir("=t", src.as_bytes(), &Default::default())
        .unwrap_or_else(|d| panic!("{d:?}"));
    let main = vm.load(&orbit_compiler::compile(&hir).expect("compiles"));
    vm.call(main, &[])
}

fn traceback(src: &str) -> String {
    let e = run(src).expect_err("the program fails");
    e.traceback
        .expect("an uncaught error has a traceback")
        .into()
}

const TBC: &str = "local function tbc(f) return setmetatable({}, {__close = f}) end\n";

#[test]
fn traceback_names_the_frames_that_raised() {
    let tb = traceback(
        "local function raiser() local x = nil; return x.y end
         local function caller() raiser() end
         caller()",
    );
    assert!(tb.starts_with("stack traceback:"), "{tb}");
    assert!(tb.contains("t:1: in upvalue 'raiser'"), "{tb}");
    assert!(tb.contains("t:2: in local 'caller'"), "{tb}");
}

/// To-be-closed variables close after the frames above the catcher are
/// gone, so the traceback is taken first and waits for the error to leave.
#[test]
fn traceback_is_taken_before_variables_close() {
    let tb = traceback(&format!(
        "{TBC}local function raiser()
           local v <close> = tbc(function() end)
           local x = nil; return x.y
         end
         local function caller()
           local w <close> = tbc(function() end)
           raiser()
         end
         caller()"
    ));
    assert!(tb.contains("t:4: in upvalue 'raiser'"), "{tb}");
    assert!(tb.contains("t:8: in local 'caller'"), "{tb}");
}

/// Errors raised and caught while the handlers run have nothing to do with
/// the waiting traceback.
#[test]
fn errors_inside_close_handlers_leave_the_traceback_alone() {
    let tb = traceback(&format!(
        "{TBC}local function fails() local x = nil; return x.z end
         local function raiser()
           local a <close> = tbc(function() pcall(string.gsub, 'a', 'a', fails) end)
           local b <close> = tbc(function() pcall(table.sort, {{2, 1}}, fails) end)
           local c <close> = tbc(function() pcall(fails) end)
           local d <close> = tbc(function() pcall(tostring, setmetatable({{}}, {{__tostring = fails}})) end)
           local e <close> = tbc(function() coroutine.wrap(function() pcall(fails) end)() end)
           local f <close> = tbc(function() assert(not load(fails)) end)
           local x = nil; return x.y
         end
         raiser()"
    ));
    assert!(tb.contains("t:10: in local 'raiser'"), "{tb}");
    assert!(!tb.contains("'fails'"), "{tb}");
}

/// Nor have errors that leave through a call a native made and waited for,
/// as host functions do.
#[test]
fn errors_through_native_calls_inside_close_handlers_leave_the_traceback_alone() {
    let tb = traceback(&format!(
        "{TBC}local function fails() local x = nil; return x.z end
         local function raiser()
           local a <close> = tbc(function() assert(not pcall(callnow, fails)) end)
           local b <close> = tbc(function() callnow(pcall, callnow, fails) end)
           local x = nil; return x.y
         end
         raiser()"
    ));
    assert!(tb.contains("t:6: in local 'raiser'"), "{tb}");
    assert!(!tb.contains("'fails'"), "{tb}");
}

/// An error raised by a `__close` handler replaces the one unwinding.
#[test]
fn a_failing_close_handler_replaces_the_error() {
    let mut vm = Vm::new();
    orbit_runtime::stdlib::open_libs(&mut vm);
    let src = format!(
        "{TBC}local function raiser()
           local v <close> = tbc(function() error('from close', 0) end)
           error('first', 0)
         end
         raiser()"
    );
    let hir = orbit_lua_syntax::to_hir("=t", src.as_bytes(), &Default::default())
        .unwrap_or_else(|d| panic!("{d:?}"));
    let main = vm.load(&orbit_compiler::compile(&hir).expect("compiles"));
    let e = vm.call(main, &[]).expect_err("fails");
    let Value::Str(m) = e.value else {
        panic!("{:?}", e.value)
    };
    assert_eq!(vm.heap.get(m).as_bytes(), b"from close");
    let tb = e.traceback.expect("a traceback");
    assert!(tb.starts_with("stack traceback:"), "{tb}");
}
