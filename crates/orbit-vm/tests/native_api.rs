//! The native-function API: stack window, continuations, yields and protected calls.

use orbit_core::Value;
use orbit_vm::baselib::{open_base, open_coroutine};
use orbit_vm::{Action, KStatus, LuaResult, Status, Vm};

fn vm() -> Vm {
    let mut vm = Vm::new();
    open_base(&mut vm);
    open_coroutine(&mut vm);
    vm
}

/// Runs `src` and returns its results as display strings.
fn run(vm: &mut Vm, src: &str) -> Result<Vec<String>, String> {
    let base = vm.top();
    let f = vm
        .load(src.as_bytes(), b"=test")
        .map_err(|e| show(vm, e.value))?;
    vm.push(f);
    match vm.pcall(0, None, 0) {
        Ok(()) => {
            let n = vm.top() - base;
            let mut out = Vec::new();
            for i in 1..=n {
                let v = vm.get(base + i);
                out.push(show(vm, v));
            }
            vm.set_top(base);
            Ok(out)
        }
        Err(e) => Err(show(vm, e.value)),
    }
}

fn show(vm: &mut Vm, v: Value) -> String {
    let s = vm.to_display(v).expect("tostring");
    String::from_utf8_lossy(vm.heap.string(s).as_bytes()).into_owned()
}

fn set_global(vm: &mut Vm, name: &str, v: Value) {
    let g = vm.globals();
    vm.heap.raw_set_name(g, name.as_bytes(), v).unwrap();
}

#[test]
fn window_and_results() {
    fn swap_add(vm: &mut Vm) -> LuaResult<Action> {
        let a = vm.check_integer(1)?;
        let b = vm.check_integer(2)?;
        vm.push(Value::Int(b));
        vm.push(Value::Int(a));
        vm.push(Value::Int(a + b));
        Ok(Action::Return(3))
    }
    let mut vm = vm();
    let f = vm.register("swap_add", swap_add);
    set_global(&mut vm, "swap_add", f);
    assert_eq!(
        run(&mut vm, "return swap_add(1, 2)").unwrap(),
        ["2", "1", "3"]
    );
    assert_eq!(
        run(&mut vm, "return swap_add(1)").unwrap_err(),
        "test:1: bad argument #2 to 'swap_add' (number expected, got no value)"
    );
    assert_eq!(
        run(&mut vm, "return swap_add(1, 2.5)").unwrap_err(),
        "test:1: bad argument #2 to 'swap_add' (number has no integer representation)"
    );
}

#[test]
fn native_closure_upvalues() {
    fn counter(vm: &mut Vm) -> LuaResult<Action> {
        let Value::Int(n) = vm.upvalue(1) else {
            unreachable!()
        };
        vm.set_upvalue(1, Value::Int(n + 1));
        vm.push(Value::Int(n + 1));
        Ok(Action::Return(1))
    }
    let mut vm = vm();
    let c = vm.native_closure("counter", counter, vec![Value::Int(10)]);
    set_global(&mut vm, "counter", c);
    assert_eq!(
        run(&mut vm, "counter() counter() return counter()").unwrap(),
        ["13"]
    );
}

/// `map(f, ...)` calls `f` on each argument through `Action::Call`, so `f` may yield.
/// `ctx` counts the results so far, which sit above the arguments.
fn map(vm: &mut Vm) -> LuaResult<Action> {
    map_step(vm, KStatus::Ok, 0)
}

fn map_step(vm: &mut Vm, _status: KStatus, done: isize) -> LuaResult<Action> {
    let done = done as usize;
    let n = vm.top() - 1 - done;
    if done == n {
        return Ok(Action::Return(n));
    }
    let f = vm.get(1);
    let x = vm.get(2 + done);
    vm.push(f);
    vm.push(x);
    Ok(Action::Call {
        nargs: 1,
        nresults: Some(1),
        k: map_step,
        ctx: done as isize + 1,
    })
}

#[test]
fn continuation_survives_yield() {
    let mut vm = vm();
    let f = vm.register("map", map);
    set_global(&mut vm, "map", f);
    let out = run(
        &mut vm,
        r#"
        local co = coroutine.wrap(function ()
          return map(function (x) return coroutine.yield(x) * 10 end, 1, 2, 3)
        end)
        local a = co()      -- yields 1
        local b = co(a)     -- resumes with 1, yields 2
        local c = co(b)     -- yields 3
        return co(c)        -- finishes: 10, 20, 30
        "#,
    )
    .unwrap();
    assert_eq!(out, ["10", "20", "30"]);
    assert_eq!(
        run(&mut vm, "return map(tostring, 1, 2)").unwrap(),
        ["1", "2"]
    );
}

#[test]
fn yield_with_continuation() {
    fn ask(vm: &mut Vm) -> LuaResult<Action> {
        vm.push(Value::Int(100));
        Ok(Action::Yield {
            n: 1,
            k: Some((answered, 7)),
        })
    }
    fn answered(vm: &mut Vm, status: KStatus, ctx: isize) -> LuaResult<Action> {
        assert_eq!(status, KStatus::Yielded);
        let got = vm.get(vm.top());
        let Value::Int(n) = got else { unreachable!() };
        vm.push(Value::Int(n + ctx as i64));
        Ok(Action::Return(1))
    }
    let mut vm = vm();
    let f = vm.register("ask", ask);
    set_global(&mut vm, "ask", f);
    let out = run(
        &mut vm,
        "local co = coroutine.create(function () return ask() end)
         local _, y = coroutine.resume(co)
         return y, coroutine.resume(co, 5)",
    )
    .unwrap();
    assert_eq!(out, ["100", "true", "12"]);
}

#[test]
fn reentrant_calls_cannot_yield() {
    fn call_now(vm: &mut Vm) -> LuaResult<Action> {
        vm.call(vm.top() - 1, None)?;
        Ok(Action::Return(vm.top()))
    }
    let mut vm = vm();
    let f = vm.register("call_now", call_now);
    set_global(&mut vm, "call_now", f);
    assert_eq!(
        run(&mut vm, "return call_now(function (a) return a + 1 end, 1)").unwrap(),
        ["2"]
    );
    let out = run(
        &mut vm,
        "local co = coroutine.create(function () call_now(coroutine.yield) end)
         return coroutine.resume(co)",
    )
    .unwrap();
    assert_eq!(out, ["false", "attempt to yield across a C-call boundary"]);
}

#[test]
fn protected_calls_from_rust() {
    let mut vm = vm();
    let f = vm.load(b"error('oops')", b"=chunk").unwrap();
    vm.push(f);
    let e = vm.pcall(0, Some(0), 0).unwrap_err();
    assert_eq!(e.status, Status::Runtime);
    assert_eq!(show(&mut vm, e.value), "chunk:1: oops");
    assert_eq!(vm.top(), 0);

    let e = vm.load(b"x = = 1", b"=chunk").unwrap_err();
    assert_eq!(e.status, Status::Syntax);
    assert_eq!(
        show(&mut vm, e.value),
        "chunk:1: unexpected symbol near '='"
    );
}

#[test]
fn pcall_action_catches_errors() {
    fn guarded(vm: &mut Vm) -> LuaResult<Action> {
        Ok(Action::PCall {
            nargs: vm.top() - 1,
            nresults: Some(1),
            handler: 0,
            k: |vm, status, _| {
                let ok = !matches!(status, KStatus::Error(_));
                let v = vm.get(vm.top());
                vm.push(Value::Bool(ok));
                vm.push(v);
                Ok(Action::Return(2))
            },
            ctx: 0,
        })
    }
    let mut vm = vm();
    let f = vm.register("guarded", guarded);
    set_global(&mut vm, "guarded", f);
    assert_eq!(
        run(&mut vm, "return guarded(function () return 'fine' end)").unwrap(),
        ["true", "fine"]
    );
    assert_eq!(
        run(&mut vm, "return guarded(error, 'bad', 0)").unwrap(),
        ["false", "bad"]
    );
    // Closing variables run before the continuation sees the error.
    assert_eq!(
        run(
            &mut vm,
            "local log = ''
             local ok, e = guarded(function ()
               local x <close> = setmetatable({}, {__close = function () log = log .. 'closed ' end})
               error('e', 0)
             end)
             return log .. e"
        )
        .unwrap(),
        ["closed e"]
    );
}

#[test]
fn deep_recursion_is_an_error_not_a_crash() {
    let mut vm = vm();
    let err = run(&mut vm, "local function f() return 1 + f() end return f()").unwrap_err();
    assert!(err.ends_with("stack overflow"), "{err}");
    let err = run(
        &mut vm,
        "local t = setmetatable({}, {}) getmetatable(t).__index = function (t, k) return t[k] end return t.x",
    )
    .unwrap_err();
    assert!(err.contains("stack overflow"), "{err}");
}
