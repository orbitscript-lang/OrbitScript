//! The parts of the debug library (`ldblib.c`) that need no introspection of the
//! interpreter: metatables, the registry, user values and `traceback`.
//!
//! `getinfo`, `getlocal`, `setlocal`, `getupvalue`, `setupvalue`, `upvalueid`,
//! `upvaluejoin`, `sethook` and `gethook` need access to frames and closures, so
//! they come from [`orbit_vm::dblib`].

use orbit_core::Value;
use orbit_vm::{Action, LuaResult, Vm};

/// Opens the debug library.
pub fn open_debug(vm: &mut Vm) {
    let mut funcs: Vec<(&'static str, orbit_vm::NativeFn)> = vec![
        ("getmetatable", db_getmetatable),
        ("getregistry", db_getregistry),
        ("getuservalue", db_getuservalue),
        ("setmetatable", db_setmetatable),
        ("setuservalue", db_setuservalue),
        ("traceback", db_traceback),
    ];
    // Introspection and hooks live in the VM crate.
    funcs.extend_from_slice(orbit_vm::dblib::FUNCTIONS);
    vm.new_lib("debug", &funcs);
}

fn db_getregistry(vm: &mut Vm) -> LuaResult<Action> {
    let r = vm.registry();
    vm.push(Value::Table(r));
    Ok(Action::Return(1))
}

fn db_getmetatable(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.check_any(1)?;
    let mt = vm.heap.metatable(v).map_or(Value::Nil, Value::Table);
    vm.push(mt);
    Ok(Action::Return(1))
}

fn db_setmetatable(vm: &mut Vm) -> LuaResult<Action> {
    let mt = match vm.arg(2) {
        Value::Nil => None,
        Value::Table(t) => Some(t),
        _ => return Err(vm.type_arg_error(2, "nil or table")),
    };
    match vm.check_any(1)? {
        Value::Table(t) => vm.heap.set_table_metatable(t, mt),
        Value::UserData(u) => vm.heap.set_userdata_metatable(u, mt),
        v => vm.heap.set_type_metatable(v.type_of(), mt),
    }
    vm.set_top(1);
    Ok(Action::Return(1))
}

fn db_getuservalue(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.opt_integer(2, 1)?;
    let Value::UserData(u) = vm.arg(1) else {
        vm.push(Value::Nil);
        return Ok(Action::Return(1));
    };
    let uv = &vm.heap.get(u).user_values;
    match usize::try_from(n).ok().and_then(|n| n.checked_sub(1)) {
        Some(i) if i < uv.len() => {
            let v = uv[i];
            vm.push(v);
            vm.push(Value::Bool(true));
            Ok(Action::Return(2))
        }
        _ => {
            vm.push(Value::Nil);
            Ok(Action::Return(1))
        }
    }
}

fn db_setuservalue(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.opt_integer(3, 1)?;
    let Value::UserData(u) = vm.arg(1) else {
        return Err(vm.type_arg_error(1, "userdata"));
    };
    let v = vm.check_any(2)?;
    vm.set_top(2);
    let ok = vm.heap.update(u, |ud| {
        match usize::try_from(n).ok().and_then(|n| n.checked_sub(1)) {
            Some(i) if i < ud.user_values.len() => {
                ud.user_values[i] = v;
                true
            }
            _ => false,
        }
    });
    if !ok {
        vm.push(Value::Nil);
    }
    Ok(Action::Return(1))
}

fn db_traceback(vm: &mut Vm) -> LuaResult<Action> {
    // A thread argument selects another coroutine's stack.
    let (arg, co) = match vm.arg(1) {
        Value::Thread(t) => (1, t),
        _ => (0, vm.current_thread()),
    };
    let other = co != vm.current_thread();
    let msg = vm.arg(arg + 1);
    let text = match msg {
        Value::Str(s) => Some(vm.heap.string(s).as_bytes().to_vec()),
        Value::Int(_) | Value::Float(_) => Some(orbit_vm::number::fmt_number(msg).into_bytes()),
        Value::Nil => None,
        _ => {
            vm.push(msg);
            return Ok(Action::Return(1));
        }
    };
    let level = vm.opt_integer(arg + 2, if other { 0 } else { 1 })?;
    let tb = vm.thread_traceback(co, text.as_deref(), usize::try_from(level).unwrap_or(0));
    let v = vm.heap.str_value(tb);
    vm.push(v);
    Ok(Action::Return(1))
}
