//! Globals, fields, indexing, method lookup and table construction.
//!
//! A field or global access carries an inline cache: the slot its key was
//! last found in. The handler takes the cache hit; `*_uncached` does the
//! lookup, refills the cache, and deals with metatables and errors.

use super::regs::{Ip, Regs, running_code, write_packed, write_tagged};
use super::{Step, leave, next, next_after_allocating, with_known_variant};
use crate::object::table::{self, IntStore};
use crate::object::{HashSeed, Tag, Value};
use crate::vm::Vm;
use crate::vm::cold::{env_error, get_str_miss, set_str_miss};
use crate::vm::dispatch::{cached_table, method_lookup};
use crate::vm::interp::Window;
use crate::vm::ops::{self, Exc};
use orbit_bytecode::{MULTI, Op};

/// `R[dst] = _ENV[K[name]]`
#[inline(never)]
pub(super) fn get_global(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GetGlobal { dst, ic, .. } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(&env_cell) = vm.heap.get(vm.rt.closure).captures.first() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Value::Table(env) = vm.heap.get(env_cell).value else {
        return get_global_uncached(vm, regs, ip);
    };
    let Some(cache) = running_code(&vm.rt).caches.get(ic as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let env = vm.heap.get(env);
    match env.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, payload)) if tag != Tag::Nil || env.meta.is_none() => {
            write_packed(&mut regs.window()[dst], tag, payload)
        }
        _ => return get_global_uncached(vm, regs, ip),
    }
    next(vm, regs, ip)
}

#[inline(never)]
fn get_global_uncached(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::GetGlobal { dst, name, ic } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let code = running_code(&vm.rt);
    let (Some(cache), Some(&Value::Str(name))) =
        (code.caches.get(ic as usize), code.consts.get(name as usize))
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let env_cell = vm.heap.get(vm.rt.closure).captures[0];
    let exc = match vm.heap.get(env_cell).value {
        Value::Table(env) => match get_str_miss(&vm.heap, &vm.rt.metas, env, name, cache) {
            Ok(v) => {
                regs.window()[dst] = v;
                return next(vm, regs, ip);
            }
            Err(exc) => exc,
        },
        other => env_error(&vm.heap, &vm.rt.metas, other),
    };
    leave(vm, regs, ip).raise_indexing(vm, dst, exc)
}

/// `R[dst] = {}`, sized for the hints.
#[inline(never)]
pub(super) fn new_table(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::NewTable {
        dst,
        array_hint,
        hash_hint,
    } = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let t = vm.op_new_table(array_hint as usize, hash_hint as usize);
    regs.window()[dst] = Value::Table(t);
    next_after_allocating(vm, regs, ip)
}

/// `R[dst] = R[obj][K[key]]`
#[inline(never)]
pub(super) fn get_field(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GetField { dst, obj, ic, .. } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let &Value::Table(t) = &r[obj] else {
        return get_field_uncached(vm, regs, ip);
    };
    let Some(cache) = running_code(&vm.rt).caches.get(ic as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let t = vm.heap.get(t);
    match t.cached_str_packed(cache.slot.get() as usize, cache.key) {
        Some((tag, payload)) if tag != Tag::Nil || t.meta.is_none() => {
            write_packed(&mut r[dst], tag, payload)
        }
        _ => return get_field_uncached(vm, regs, ip),
    }
    next(vm, regs, ip)
}

#[inline(never)]
fn get_field_uncached(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::GetField { dst, obj, key, ic } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let code = running_code(&vm.rt);
    let (Some(cache), Some(&key)) = (code.caches.get(ic as usize), code.consts.get(key as usize))
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let exc = match (r[obj], key) {
        (Value::Table(t), Value::Str(k)) => {
            match get_str_miss(&vm.heap, &vm.rt.metas, t, k, cache) {
                Ok(v) => {
                    r[dst] = v;
                    return next(vm, regs, ip);
                }
                Err(exc) => exc,
            }
        }
        (other, key) => Exc::index(other, key),
    };
    leave(vm, regs, ip).raise_indexing(vm, dst, exc)
}

/// `R[obj][K[key]] = R[src]`
#[inline(never)]
pub(super) fn set_field(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SetField { obj, src, ic, .. } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let &Value::Table(t) = &r[obj] else {
        return set_field_uncached(vm, regs, ip);
    };
    let Some(cache) = running_code(&vm.rt).caches.get(ic as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let slot = cache.slot.get() as usize;
    let table = vm.heap.get(t);
    let hit = matches!(table.cached_str_packed(slot, cache.key),
        Some((tag, _)) if table.meta.is_none() || tag != Tag::Nil);
    if !hit {
        return set_field_uncached(vm, regs, ip);
    }
    // A number needs no write barrier, and so no call.
    let v = match r[src] {
        Value::Int(n) => Value::Int(n),
        Value::Float(f) => Value::Float(f),
        _ => return set_field_with_barrier(vm, regs, ip),
    };
    vm.heap.write_for(t, &v).set_slot(slot, v);
    next(vm, regs, ip)
}

/// [`set_field`] on a cache hit, for a value that may need the write
/// barrier.
#[inline(never)]
fn set_field_with_barrier(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::SetField { obj, src, ic, .. } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let (&Value::Table(t), Some(cache)) = (&r[obj], running_code(&vm.rt).caches.get(ic as usize))
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let slot = cache.slot.get() as usize;
    with_known_variant(&r[src], |v| vm.heap.write_for(t, &v).set_slot(slot, v));
    next(vm, regs, ip)
}

#[inline(never)]
fn set_field_uncached(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::SetField { obj, key, src, ic } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let code = running_code(&vm.rt);
    let (Some(cache), Some(&key)) = (code.caches.get(ic as usize), code.consts.get(key as usize))
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let v = r[src];
    let exc = match (r[obj], key) {
        (Value::Table(t), Value::Str(k)) => {
            match set_str_miss(&mut vm.heap, &vm.rt.metas, t, k, v, cache) {
                Ok(()) => return next_after_allocating(vm, regs, ip),
                Err(exc) => exc,
            }
        }
        (other, key) => Exc::new_index(other, key, v),
    };
    leave(vm, regs, ip).raise(vm, exc)
}

/// `R[dst] = R[obj][R[key]]`
#[inline(never)]
pub(super) fn get_index(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GetIndex { dst, obj, key } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        // The array part.
        if let Some((tag, payload)) = vm.heap.get(t).read_int_raw(k) {
            write_tagged(&mut r[dst], tag, payload);
            return next(vm, regs, ip);
        }
    }
    get_index_general(vm, regs, ip)
}

/// An indexed read outside the array part, or with another kind of key or
/// object.
#[inline(never)]
fn get_index_general(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::GetIndex { dst, obj, key } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let key = r[key];
    let exc = match r[obj] {
        Value::Table(t) => {
            let table = vm.heap.get(t);
            let v = match key {
                Value::Int(k) => table.get_int(HashSeed::of(&vm.heap), k),
                k => table.get(&vm.heap, k),
            };
            if !v.is_nil() || table.meta.is_none() {
                r[dst] = v;
                return next(vm, regs, ip);
            }
            Exc::index(Value::Table(t), key)
        }
        other => Exc::index(other, key),
    };
    leave(vm, regs, ip).raise_indexing(vm, dst, exc)
}

/// `R[obj][R[key]] = R[src]`
#[inline(never)]
pub(super) fn set_index(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SetIndex { obj, key, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    if let (&Value::Table(t), &Value::Int(k)) = (&r[obj], &r[key]) {
        // A number or a boolean into the array part needs no write
        // barrier, and so no call.
        let v = match r[src] {
            Value::Int(n) => Value::Int(n),
            Value::Float(f) => Value::Float(f),
            Value::Bool(b) => Value::Bool(b),
            _ => return set_index_general(vm, regs, ip),
        };
        if table::store_in_array(&mut vm.heap, t, k, v) {
            return next(vm, regs, ip);
        }
    }
    set_index_general(vm, regs, ip)
}

#[inline(never)]
fn set_index_general(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::SetIndex { obj, key, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let (object, key, v) = (r[obj], r[key], r[src]);
    let exc = match (object, key) {
        (Value::Table(t), Value::Int(k)) => match table::store_int(&mut vm.heap, t, k, v) {
            IntStore::Array => return next(vm, regs, ip),
            IntStore::Other => return next_after_allocating(vm, regs, ip),
            IntStore::Slow => Exc::new_index(object, key, v),
        },
        (Value::Table(t), _) if vm.heap.get(t).meta.is_none() => {
            match table::set(&mut vm.heap, t, key, v) {
                Ok(_) => return next_after_allocating(vm, regs, ip),
                Err(e) => ops::store_failed(e, t, key, v),
            }
        }
        _ => Exc::new_index(object, key, v),
    };
    leave(vm, regs, ip).raise(vm, exc)
}

/// `R[dst] = R[obj][key]` for a small integer key.
#[inline(never)]
pub(super) fn get_index_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GetIndexInt { dst, obj, key } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    if let &Value::Table(t) = &r[obj] {
        if let Some((tag, payload)) = vm.heap.get(t).read_int_raw(key as i64) {
            write_tagged(&mut r[dst], tag, payload);
            return next(vm, regs, ip);
        }
    }
    get_index_int_general(vm, regs, ip)
}

#[inline(never)]
fn get_index_int_general(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::GetIndexInt { dst, obj, key } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let key = key as i64;
    let exc = match r[obj] {
        Value::Table(t) => {
            let table = vm.heap.get(t);
            let v = table.get_int(HashSeed::of(&vm.heap), key);
            if !v.is_nil() || table.meta.is_none() {
                r[dst] = v;
                return next(vm, regs, ip);
            }
            Exc::index(Value::Table(t), Value::Int(key))
        }
        other => Exc::index(other, Value::Int(key)),
    };
    leave(vm, regs, ip).raise_indexing(vm, dst, exc)
}

/// `R[obj][key] = R[src]` for a small integer key.
#[inline(never)]
pub(super) fn set_index_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SetIndexInt { obj, key, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    if let &Value::Table(t) = &r[obj] {
        let v = match r[src] {
            Value::Int(n) => Value::Int(n),
            Value::Float(f) => Value::Float(f),
            Value::Bool(b) => Value::Bool(b),
            _ => return set_index_int_general(vm, regs, ip),
        };
        if table::store_in_array(&mut vm.heap, t, key as i64, v) {
            return next(vm, regs, ip);
        }
    }
    set_index_int_general(vm, regs, ip)
}

#[inline(never)]
fn set_index_int_general(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::SetIndexInt { obj, key, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let (object, key, v) = (r[obj], key as i64, r[src]);
    if let Value::Table(t) = object {
        match table::store_int(&mut vm.heap, t, key, v) {
            IntStore::Array => return next(vm, regs, ip),
            IntStore::Other => return next_after_allocating(vm, regs, ip),
            IntStore::Slow => {}
        }
    }
    let exc = Exc::new_index(object, Value::Int(key), v);
    leave(vm, regs, ip).raise(vm, exc)
}

/// `R[table][start..] = R[first..first+count]`. Grows the table, so it
/// works on the whole VM.
#[inline(never)]
pub(super) fn set_list(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SetList {
        table,
        first,
        count,
        start,
    } = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    let Value::Table(t) = Window::take(&mut vm.rt.stack.regs, at.base)[table] else {
        unreachable!("SetList on a table")
    };
    let from = at.base + first as usize;
    let n = if count == MULTI {
        vm.rt.top - from
    } else {
        count as usize
    };
    match vm.op_set_list(t, from, n, start as i64) {
        Ok(()) => at.done_after_allocating(vm),
        Err(e) => at.raise(vm, e.into()),
    }
}

/// `R[dst+1] = R[obj]; R[dst] = R[obj][K[key]]`: a method and its object,
/// ready to call.
#[inline(never)]
pub(super) fn self_call(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SelfCall { dst, obj, ic, .. } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    // Two caches: one for the method's slot, one for the slot of
    // `__index` in the object's metatable.
    let ic = ic as usize;
    let (Value::Table(t), Some([method_cache, index_cache])) =
        (r[obj], running_code(&vm.rt).caches.get(ic..ic + 2))
    else {
        return self_call_uncached(vm, regs, ip);
    };
    let heap = &vm.heap;
    let object = heap.get(t);
    let slot = method_cache.slot.get() as usize;
    // The method is in the object itself, or in the table its
    // metatable's `__index` names.
    let method = match object.cached_str_packed(slot, method_cache.key) {
        Some(method) if method.0 != Tag::Nil => method,
        Some(_) => return self_call_uncached(vm, regs, ip),
        None => {
            let Some(metatable) = object.meta else {
                return self_call_uncached(vm, regs, ip);
            };
            let Some(class) = cached_table(heap.get(metatable), index_cache) else {
                return self_call_uncached(vm, regs, ip);
            };
            match heap.get(class).cached_str_packed(slot, method_cache.key) {
                Some(method) if method.0 != Tag::Nil => method,
                _ => return self_call_uncached(vm, regs, ip),
            }
        }
    };
    r[dst + 1] = Value::Table(t);
    write_packed(&mut r[dst], method.0, method.1);
    next(vm, regs, ip)
}

#[inline(never)]
fn self_call_uncached(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::SelfCall { dst, obj, key, ic } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let code = running_code(&vm.rt);
    let Some(&Value::Str(name)) = code.consts.get(key as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let object = r[obj];
    r[dst + 1] = object;
    if let Value::Table(t) = object {
        if let Some((tag, payload)) = method_lookup(&vm.heap, &vm.rt.metas, code, ic, name, t) {
            write_packed(&mut r[dst], tag, payload);
            return next(vm, regs, ip);
        }
    }
    let exc = Exc::index(object, Value::Str(name));
    leave(vm, regs, ip).raise_indexing(vm, dst, exc)
}
