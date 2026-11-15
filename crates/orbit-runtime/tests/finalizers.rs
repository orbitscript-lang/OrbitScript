//! Running `__gc` finalizers from Rust: order, errors, closing.

use orbit_runtime::object::{Gc, Table, Value};
use orbit_runtime::vm::{LuaError, NativeCtx, NativeReturn, Vm};

/// A finalizer that appends its object's `n` field to the global `log`.
fn record(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let obj = ctx.check_table(0)?;
    let n_key = ctx.new_str(b"n");
    let n = ctx.vm.table_get(obj, n_key);
    let Value::Table(log) = ctx.vm.global("log") else {
        unreachable!()
    };
    let len = ctx.vm.table_len(log);
    ctx.vm.table_set(log, Value::Int(len + 1), n);
    Ok(NativeReturn::Return(0))
}

fn fails(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    Err(ctx.error("boom"))
}

fn setup() -> (Vm, Gc<Table>) {
    let mut vm = Vm::new();
    let log = vm.new_table();
    vm.set_global("log", Value::Table(log));
    let mt = vm.new_table();
    vm.set_global("mt", Value::Table(mt));
    let f = vm.register_native("record", record);
    let k = vm.new_str(b"__gc");
    vm.table_set(mt, k, f);
    (vm, mt)
}

/// A table with field `n` and metatable `mt`, registered for `__gc` and
/// not rooted.
fn object(vm: &mut Vm, mt: Gc<Table>, n: i64) -> Gc<Table> {
    let t = vm.new_table();
    let k = vm.new_str(b"n");
    vm.table_set(t, k, Value::Int(n));
    assert!(vm.set_metatable(Value::Table(t), Some(mt)));
    t
}

fn log(vm: &mut Vm) -> Vec<i64> {
    let Value::Table(log) = vm.global("log") else {
        unreachable!()
    };
    (1..=vm.table_len(log))
        .map(|i| match vm.table_get(log, Value::Int(i)) {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect()
}

#[test]
fn finalizers_run_newest_first_once() {
    let (mut vm, mt) = setup();
    for n in 1..=4 {
        object(&mut vm, mt, n);
    }
    vm.full_collect();
    vm.run_finalizers();
    assert_eq!(log(&mut vm), [4, 3, 2, 1]);
    vm.full_collect();
    vm.run_finalizers();
    assert_eq!(log(&mut vm), [4, 3, 2, 1]);
}

#[test]
fn an_error_in_one_finalizer_does_not_stop_the_rest() {
    let (mut vm, mt) = setup();
    object(&mut vm, mt, 1);
    let bad_mt = vm.new_table();
    vm.pin(Value::Table(bad_mt));
    let f = vm.register_native("fails", fails);
    let k = vm.new_str(b"__gc");
    vm.table_set(bad_mt, k, f);
    let bad = vm.new_table();
    vm.set_metatable(Value::Table(bad), Some(bad_mt));
    object(&mut vm, mt, 3);
    vm.full_collect();
    vm.run_finalizers();
    assert_eq!(log(&mut vm), [3, 1]);
    assert!(!vm.gc_stopped(), "collection resumes after finalizers");
}

#[test]
fn close_finalizes_reachable_objects_once() {
    let (mut vm, mt) = setup();
    let kept = object(&mut vm, mt, 1);
    vm.pin(Value::Table(kept));
    object(&mut vm, mt, 2);
    vm.close();
    assert_eq!(log(&mut vm), [2, 1]);
    vm.close();
    assert_eq!(log(&mut vm), [2, 1]);
}
