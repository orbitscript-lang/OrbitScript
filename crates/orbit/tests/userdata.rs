//! Userdata through the embedding API: a host value with a metatable whose
//! methods are natives.

use orbit::Lua;
use orbit_vm::orbit_core::Value;
use orbit_vm::{LuaError, NativeCtx, NativeReturn};

#[derive(Debug)]
struct Counter {
    n: i64,
}

fn counter_new(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let start = ctx.opt_int(0, 0)?;
    let u = ctx.new_userdata(Counter { n: start });
    let Value::Table(mt) = ctx.arg(1) else {
        return Err(ctx.type_error(1, "table"));
    };
    ctx.heap.update(u, |u| u.meta = Some(mt));
    ctx.ret1(Value::UserData(u))
}

fn counter_inc(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let u = ctx.check_userdata::<Counter>(0, "Counter")?;
    let by = ctx.opt_int(1, 1)?;
    let c = ctx.userdata_mut::<Counter>(u).expect("a Counter");
    c.n += by;
    let n = c.n;
    ctx.ret1(Value::Int(n))
}

#[test]
fn userdata_with_native_methods() {
    let mut lua = Lua::new();
    let vm = lua.vm();
    let f = vm.register_native("counter_new", counter_new);
    vm.set_global("counter_new", f);
    let f = vm.register_native("counter_inc", counter_inc);
    vm.set_global("counter_inc", f);
    let out = lua
        .exec(
            br#"
            local mt = {__index = {inc = counter_inc}, __name = "Counter"}
            mt.__eq = function (a, b) return a:inc(0) == b:inc(0) end
            local c = counter_new(10, mt)
            c:inc(); c:inc(5)
            local same = counter_new(16, mt)
            local ok, err = pcall(counter_inc, {})
            return type(c), c:inc(0), c == same, c ~= counter_new(1, mt),
                tostring(c):sub(1, 9), err
            "#,
            "ud.lua",
        )
        .expect("runs");
    let vm = lua.vm();
    let shown: Vec<String> = out
        .iter()
        .map(|&v| String::from_utf8_lossy(&vm.to_display(v)).into_owned())
        .collect();
    assert_eq!(
        shown,
        [
            "userdata",
            "16",
            "true",
            "true",
            "Counter: ",
            "bad argument #1 to 'counter_inc' (Counter expected, got table)"
        ]
    );
}

#[test]
fn userdata_survives_a_stressed_collector() {
    let mut lua = Lua::new();
    let vm = lua.vm();
    vm.heap.set_stress(true);
    let f = vm.register_native("counter_new", counter_new);
    vm.set_global("counter_new", f);
    let f = vm.register_native("counter_inc", counter_inc);
    vm.set_global("counter_inc", f);
    let out = lua
        .exec(
            br#"
            local mt = {__index = {inc = counter_inc}}
            local all = {}
            for i = 1, 200 do all[i] = counter_new(i, mt); local _ = {} end
            local sum = 0
            for i = 1, 200 do sum = sum + all[i]:inc() end
            return sum
            "#,
            "ud.lua",
        )
        .expect("runs");
    assert!(matches!(out[..], [Value::Int(20300)]));
}
