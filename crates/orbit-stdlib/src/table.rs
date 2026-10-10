//! The `table` library.
//!
//! Each function has a raw loop for tables without a metatable, the common
//! case, and a resumable slow path in [`slow`] that honours `__index`,
//! `__newindex` and `__len` (docs/design/stdlib-string-table-math.md#table).

mod slow;
mod sort;

use orbit_vm::orbit_core::table as tbl;
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("table.insert", insert),
    ("table.remove", remove),
    ("table.concat", concat),
    ("table.unpack", unpack),
    ("table.pack", pack),
    ("table.move", move_),
    ("table.create", create),
    ("table.sort", sort::sort),
];

/// Largest array size `create` and `sort` accept (a C `int`).
const MAX_INT: i64 = i32::MAX as i64;
/// Most hash entries `create` preallocates before `table overflow`.
const MAX_HASH: i64 = 1 << 30;
/// Most slots `create` actually reserves up front; larger requests grow on
/// demand instead of failing the allocation outright.
const MAX_RESERVE: usize = 1 << 24;

fn get(ctx: &NativeCtx<'_>, t: Gc<Table>, i: i64) -> Value {
    ctx.vm.heap.get(t).get_int(i)
}

fn set(ctx: &mut NativeCtx<'_>, t: Gc<Table>, i: i64, v: Value) -> Result<(), LuaError> {
    tbl::set_int(&mut ctx.vm.heap, t, i, v).map_err(|_| ctx.no_memory())?;
    Ok(())
}

/// Argument `i` as a table without a metatable: the raw loops apply.
fn plain(ctx: &NativeCtx<'_>, i: usize) -> Option<Gc<Table>> {
    match ctx.arg(i) {
        Value::Table(t) if ctx.vm.heap.get(t).meta.is_none() => Some(t),
        _ => None,
    }
}

/// `#t` for the table functions.
fn length(ctx: &NativeCtx<'_>, t: Gc<Table>) -> i64 {
    ctx.vm.heap.get(t).border()
}

fn insert(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        return slow::insert(ctx);
    };
    let end = length(ctx, t).wrapping_add(1);
    match ctx.nargs() {
        2 => {
            let v = ctx.arg(1);
            set(ctx, t, end, v)?;
        }
        3 => {
            let pos = ctx.check_int(1)?;
            // `pos - 1` as unsigned: 1..=end is valid.
            if (pos as u64).wrapping_sub(1) >= end as u64 {
                return Err(ctx.arg_error(1, "position out of bounds"));
            }
            let mut i = end;
            while i > pos {
                let v = get(ctx, t, i - 1);
                set(ctx, t, i, v)?;
                i -= 1;
            }
            let v = ctx.arg(2);
            set(ctx, t, pos, v)?;
        }
        _ => return Err(ctx.error("wrong number of arguments to 'insert'")),
    }
    Ok(NativeReturn::Return(0))
}

fn remove(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        return slow::remove(ctx);
    };
    let size = length(ctx, t);
    let mut pos = ctx.opt_int(1, size)?;
    // `pos - 1` as unsigned: 1..=size+1 is valid when a position is given.
    if ctx.nargs() > 1 && pos != size && (pos as u64).wrapping_sub(1) > size as u64 {
        return Err(ctx.arg_error(1, "position out of bounds"));
    }
    let v = get(ctx, t, pos);
    while pos < size {
        let next = get(ctx, t, pos + 1);
        set(ctx, t, pos, next)?;
        pos += 1;
    }
    set(ctx, t, pos, Value::Nil)?;
    ctx.ret1(v)
}

fn concat(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        return slow::concat(ctx);
    };
    let sep = if ctx.arg(1).is_nil() {
        Vec::new()
    } else {
        let s = ctx.check_str(1)?;
        ctx.bytes(s).to_vec()
    };
    let i = ctx.opt_int(2, 1)?;
    let j = if ctx.arg(3).is_nil() {
        length(ctx, t)
    } else {
        ctx.check_int(3)?
    };
    let mut out = Vec::new();
    let mut k = i;
    while k <= j {
        let v = get(ctx, t, k);
        // Long pieces grow the buffer by a lot: reserve first, so running
        // out of memory is an error rather than an abort.
        let piece = match v {
            Value::Str(s) => ctx.bytes(s).len(),
            _ => 0,
        };
        if out.try_reserve(piece + sep.len()).is_err() {
            return Err(ctx.no_memory());
        }
        match v {
            Value::Str(_) | Value::Int(_) | Value::Float(_) => ctx.display(v, &mut out),
            _ => {
                return Err(ctx.error(format!(
                    "invalid value ({}) at index {k} in table for 'concat'",
                    v.type_name()
                )));
            }
        }
        if k == j {
            break;
        }
        out.extend_from_slice(&sep);
        k += 1;
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

fn unpack(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        return slow::unpack(ctx);
    };
    let i = ctx.opt_int(1, 1)?;
    let j = if ctx.arg(2).is_nil() {
        length(ctx, t)
    } else {
        ctx.check_int(2)?
    };
    if i > j {
        return Ok(NativeReturn::Return(0));
    }
    let n = (j as i128 - i as i128 + 1) as u128;
    let room = ctx.vm.rt.limits.max_stack.saturating_sub(ctx.len()) as u128;
    if n >= MAX_INT as u128 || n >= room {
        return Err(ctx.error("too many results to unpack"));
    }
    let mut k = i;
    loop {
        let v = get(ctx, t, k);
        ctx.push(v);
        if k == j {
            break;
        }
        k += 1;
    }
    Ok(NativeReturn::Return(n as usize))
}

fn pack(ctx: &mut NativeCtx<'_>) -> R {
    let n = ctx.nargs();
    let t = ctx.new_table(Table::with_capacity(n, 1));
    // Root the table in the window before allocating the key.
    ctx.push(Value::Table(t));
    for i in 0..n {
        let v = ctx.arg(i);
        set(ctx, t, i as i64 + 1, v)?;
    }
    let k = ctx.new_str(b"n");
    let _ = tbl::set(&mut ctx.vm.heap, t, k, Value::Int(n as i64));
    Ok(NativeReturn::Return(1))
}

fn move_(ctx: &mut NativeCtx<'_>) -> R {
    let (Some(a1), true) = (
        plain(ctx, 0),
        ctx.arg(4).is_nil() || plain(ctx, 4).is_some(),
    ) else {
        return slow::move_(ctx);
    };
    let f = ctx.check_int(1)?;
    let e = ctx.check_int(2)?;
    let t = ctx.check_int(3)?;
    let a2 = plain(ctx, 4).unwrap_or(a1);
    if e >= f {
        if !(f > 0 || e < i64::MAX.wrapping_add(f)) {
            return Err(ctx.arg_error(2, "too many elements to move"));
        }
        let n = e - f;
        if t > i64::MAX - n {
            return Err(ctx.arg_error(3, "destination wrap around"));
        }
        let overlapping = a1 == a2 && t > f && t <= e;
        if overlapping {
            for i in (0..=n).rev() {
                let v = get(ctx, a1, f + i);
                set(ctx, a2, t + i, v)?;
            }
        } else {
            for i in 0..=n {
                let v = get(ctx, a1, f + i);
                set(ctx, a2, t + i, v)?;
            }
        }
    }
    ctx.ret1(Value::Table(a2))
}

fn create(ctx: &mut NativeCtx<'_>) -> R {
    let n = ctx.check_int(0)?;
    if !(0..=MAX_INT).contains(&n) {
        return Err(ctx.arg_error(0, "out of range"));
    }
    let m = ctx.opt_int(1, 0)?;
    if !(0..=MAX_INT).contains(&m) {
        return Err(ctx.arg_error(1, "out of range"));
    }
    if m > MAX_HASH {
        return Err(ctx.error("table overflow"));
    }
    let (n, m) = ((n as usize).min(MAX_RESERVE), (m as usize).min(MAX_RESERVE));
    // About what the parts take: 9 bytes an array slot, a hash entry plus
    // load-factor slack.
    ctx.reserve(n.saturating_mul(9).saturating_add(m.saturating_mul(32)))?;
    let Ok(t) = Table::try_with_capacity(n, m) else {
        return Err(ctx.no_memory());
    };
    let t = ctx.new_table(t);
    ctx.ret1(Value::Table(t))
}
