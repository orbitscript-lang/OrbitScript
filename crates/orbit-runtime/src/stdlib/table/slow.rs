//! The table functions on values with metatables: every read, write and
//! length goes through `__index`, `__newindex` and `__len`, in the order
//! lua5.5 makes them (found by logging metamethod calls on proxies).
//!
//! Metamethods run nested ([`NativeCtx::call_want`]), so each function is a
//! plain loop; as in the reference, nothing may yield across them.

use crate::object::Value;
use crate::vm::{LuaError, MetaField, NativeCtx, NativeReturn};

use super::{MAX_INT, R};

/// A length as the table functions take it: an integer, a float with an
/// integral value, or a string that converts to one.
fn len_int(ctx: &mut NativeCtx<'_>, v: Value) -> Result<i64, LuaError> {
    match ctx.to_integer(v) {
        Some(n) => Ok(n),
        None => Err(ctx.error("object length is not an integer")),
    }
}

/// `#t` with `__len`, as the table functions take it.
fn length(ctx: &mut NativeCtx<'_>, t: Value) -> Result<i64, LuaError> {
    let n = ctx.length_now(t)?;
    len_int(ctx, n)
}

/// Which metamethods a non-table argument must have to stand in for a
/// table.
#[derive(Debug, Clone, Copy)]
struct Needs {
    read: bool,
    write: bool,
}

const READ: Needs = Needs {
    read: true,
    write: false,
};
const WRITE: Needs = Needs {
    read: false,
    write: true,
};

/// Argument `i` is a table, or has the metamethods `needs` names.
fn check_tab(ctx: &mut NativeCtx<'_>, i: usize, needs: Needs) -> Result<Value, LuaError> {
    let v = ctx.arg(i);
    if matches!(v, Value::Table(_)) {
        return Ok(v);
    }
    let ok = ctx.metatable(v).is_some()
        && (!needs.read || !ctx.metamethod(v, MetaField::Index).is_nil())
        && (!needs.write || !ctx.metamethod(v, MetaField::NewIndex).is_nil());
    if ok {
        Ok(v)
    } else {
        Err(ctx.type_error(i, "table"))
    }
}

/// Resize the window to its first `n` slots, padding with nils, so what the
/// function pushes goes above its arguments.
fn window(ctx: &mut NativeCtx<'_>, n: usize) {
    ctx.truncate(n);
    while ctx.len() < n {
        ctx.push(Value::Nil);
    }
}

pub(super) fn concat(ctx: &mut NativeCtx<'_>) -> R {
    let t = check_tab(ctx, 0, READ)?;
    window(ctx, 4);
    // The length comes first, even when the range is given.
    let last = length(ctx, t)?;
    let sep = match ctx.arg(1) {
        Value::Nil => Vec::new(),
        _ => {
            let s = ctx.check_str(1)?;
            ctx.bytes(s).to_vec()
        }
    };
    let mut i = ctx.opt_int(2, 1)?;
    let last = ctx.opt_int(3, last)?;
    let mut out = Vec::new();
    while i <= last {
        let v = ctx.index_now(t, Value::Int(i))?;
        let piece = match v {
            Value::Str(s) => ctx.bytes(s).len(),
            Value::Int(_) | Value::Float(_) => 0,
            _ => {
                return Err(ctx.error(&format!(
                    "invalid value ({}) at index {i} in table for 'concat'",
                    v.type_name()
                )));
            }
        };
        // Long pieces grow the buffer by a lot: reserve first, so running
        // out of memory is an error rather than an abort.
        if out.try_reserve(piece + sep.len()).is_err() {
            return Err(ctx.no_memory());
        }
        ctx.display(v, &mut out);
        if i == last {
            break;
        }
        out.extend_from_slice(&sep);
        i += 1;
    }
    let r = ctx.new_str_owned(out);
    ctx.ret([r])
}

pub(super) fn unpack(ctx: &mut NativeCtx<'_>) -> R {
    let t = check_tab(ctx, 0, READ)?;
    window(ctx, 3);
    let len = length(ctx, t)?;
    let i = ctx.opt_int(1, 1)?;
    let j = ctx.opt_int(2, len)?;
    if i > j {
        return Ok(NativeReturn::Return(0));
    }
    let room = ctx.vm.rt.host.max_stack().saturating_sub(ctx.len());
    let Some(n) = j
        .checked_sub(i)
        .and_then(|d| d.checked_add(1))
        // More results than an integer holds, or the stack.
        .filter(|&n| n < MAX_INT && (n as u64) < room as u64)
    else {
        return Err(ctx.error("too many results to unpack"));
    };
    // Each value goes on the window as it arrives, rooted while the next
    // metamethods run.
    let mut k = i;
    loop {
        let v = ctx.index_now(t, Value::Int(k))?;
        ctx.push(v);
        if k == j {
            break;
        }
        k += 1;
    }
    Ok(NativeReturn::Return(n as usize))
}

pub(super) fn move_(ctx: &mut NativeCtx<'_>) -> R {
    let a1 = check_tab(ctx, 0, READ)?;
    let f = ctx.check_int(1)?;
    let e = ctx.check_int(2)?;
    let t = ctx.check_int(3)?;
    let explicit = !ctx.arg(4).is_nil();
    let a2 = if explicit {
        check_tab(ctx, 4, WRITE)?
    } else {
        a1
    };
    if e < f {
        return ctx.ret([a2]);
    }
    // The elements after the first; their count must fit an integer.
    let Some(n) = e.checked_sub(f).filter(|&n| n < i64::MAX) else {
        return Err(ctx.arg_error(2, "too many elements to move"));
    };
    if t.checked_add(n).is_none() {
        return Err(ctx.arg_error(3, "destination wrap around"));
    }
    window(ctx, 5);
    ctx.set(4, a2);
    // Moving up is safe unless the destination starts inside the source of
    // the same table (`__eq` included when the tables were given apart).
    let down = !(t > e || t <= f) && (!explicit || ctx.equal_now(a1, a2)?);
    let step = |ctx: &mut NativeCtx<'_>, i: i64| -> Result<(), LuaError> {
        let v = ctx.index_now(a1, Value::Int(f + i))?;
        ctx.set_index_now(a2, Value::Int(t + i), v)
    };
    if down {
        for i in (0..=n).rev() {
            step(ctx, i)?;
        }
    } else {
        for i in 0..=n {
            step(ctx, i)?;
        }
    }
    ctx.ret([a2])
}
