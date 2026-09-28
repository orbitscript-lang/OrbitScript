//! The parts of the debug library (`ldblib.c`) that need no introspection of the
//! interpreter: metatables, the registry, user values and `traceback`.
//!
//! `getinfo`, `getlocal`, `setlocal`, `getupvalue`, `setupvalue`, `upvalueid`,
//! `upvaluejoin`, `sethook`, `gethook` and `setcstacklimit` need access to frames
//! and closures inside `orbit-vm`, and are not provided yet.

use orbit_core::Value;
use orbit_vm::{Action, LuaResult, Vm};

/// Opens the debug library.
pub fn open_debug(vm: &mut Vm) {
    vm.new_lib(
        "debug",
        &[
            ("getmetatable", db_getmetatable),
            ("getregistry", db_getregistry),
            ("getuservalue", db_getuservalue),
            ("setmetatable", db_setmetatable),
            ("setuservalue", db_setuservalue),
            ("traceback", db_traceback),
        ],
    );
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
    // A thread argument selects another coroutine's stack. Only the running one can
    // be walked for now, so another thread's traceback lists no frames.
    let (arg, other) = match vm.arg(1) {
        Value::Thread(t) => (1, t != vm.current_thread()),
        _ => (0, false),
    };
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
    let tb = if other {
        let mut out = Vec::new();
        if let Some(t) = text {
            out.extend_from_slice(&t);
            out.push(b'\n');
        }
        out.extend_from_slice(b"stack traceback:");
        out
    } else {
        vm.traceback(text.as_deref(), usize::try_from(level).unwrap_or(0))
    };
    let v = vm.heap.str_value(tb);
    vm.push(v);
    Ok(Action::Return(1))
}
