//! The table functions on values with metatables: every read, write and
//! length goes through `__index`, `__newindex` and `__len`, in the order
//! lua5.5 makes them (found by logging metamethod calls on proxies).
//!
//! Metamethods run nested ([`NativeCtx::call_want`]), so each function is a
//! plain loop; as in the reference, nothing may yield across them.

use crate::object::{Gc, Table, Value};
use crate::vm::{LuaError, MetaField, NativeCtx, NativeReturn, Vm};

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
const READ_WRITE: Needs = Needs {
    read: true,
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

pub(super) fn insert(ctx: &mut NativeCtx<'_>) -> R {
    let t = check_tab(ctx, 0, READ_WRITE)?;
    let e = length(ctx, t)?.wrapping_add(1);
    match ctx.nargs() {
        2 => {
            let v = ctx.arg(1);
            ctx.set_index_now(t, Value::Int(e), v)?;
        }
        3 => {
            let pos = ctx.check_int(1)?;
            // `pos - 1` as unsigned: 1..=e is valid.
            if (pos.wrapping_sub(1) as u64) >= e as u64 {
                return Err(ctx.arg_error(1, "position out of bounds"));
            }
            let mut i = e;
            while i > pos {
                let v = ctx.index_now(t, Value::Int(i - 1))?;
                ctx.set_index_now(t, Value::Int(i), v)?;
                i -= 1;
            }
            let v = ctx.arg(2);
            ctx.set_index_now(t, Value::Int(pos), v)?;
        }
        _ => return Err(ctx.error("wrong number of arguments to 'insert'")),
    }
    Ok(NativeReturn::Return(0))
}

/// The largest array `sort` accepts (a C `int`).
const MAX_SORT: i64 = i32::MAX as i64;

/// `table.sort`: a stable bottom-up merge sort over a copy of the
/// elements, so each element is read and written once whatever
/// metamethods the table has. The copies live in two tables on the
/// window, where the collector sees them while comparisons run.
pub(super) fn sort(ctx: &mut NativeCtx<'_>) -> R {
    let t = check_tab(ctx, 0, READ_WRITE)?;
    let plain = match t {
        Value::Table(h) if ctx.vm.heap.get(h).meta.is_none() => Some(h),
        _ => None,
    };
    let n = match plain {
        Some(h) => ctx.vm.heap.get(h).border(),
        None => length(ctx, t)?,
    };
    if n <= 1 {
        return Ok(NativeReturn::Return(0));
    }
    if n >= MAX_SORT {
        return Err(ctx.arg_error(0, "array too big"));
    }
    let lt = ctx.arg(1);
    if !lt.is_nil() && !lt.is_function() {
        return Err(ctx.type_error(1, "function"));
    }
    window(ctx, 2);
    let mut src = new_array(ctx, n)?;
    let mut dst = new_array(ctx, n)?;
    for i in 1..=n {
        let v = match plain {
            Some(h) => ctx.vm.heap.get(h).get_int(i),
            None => ctx.index_now(t, Value::Int(i))?,
        };
        put(ctx, src, i, v)?;
    }
    if lt.is_nil() && sort_primitive(ctx, src, n)? {
        return write_back(ctx, t, plain, src, n);
    }
    let mut width = 1;
    while width < n {
        let mut lo = 1;
        while lo <= n {
            let mid = if lo + width <= n { lo + width } else { n + 1 };
            let hi = if lo + 2 * width <= n {
                lo + 2 * width
            } else {
                n + 1
            };
            let (mut i, mut j) = (lo, mid);
            for k in lo..hi {
                // Take the right element only if it is strictly smaller,
                // which keeps the sort stable.
                let right = i >= mid
                    || (j < hi && {
                        let (a, b) = (at(ctx, src, j), at(ctx, src, i));
                        less(ctx, lt, a, b)?
                    });
                let v = if right {
                    j += 1;
                    at(ctx, src, j - 1)
                } else {
                    i += 1;
                    at(ctx, src, i - 1)
                };
                put(ctx, dst, k, v)?;
            }
            lo = hi;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    // A comparator or `__lt` must be a strict order: no neighbour may come
    // out smaller than the one before it.
    for i in 2..=n {
        let (a, b) = (at(ctx, src, i), at(ctx, src, i - 1));
        if less(ctx, lt, a, b)? {
            return Err(ctx.error("invalid order function for sorting"));
        }
    }
    write_back(ctx, t, plain, src, n)
}

/// A table with room for `n` array elements, kept on the window.
fn new_array(
    ctx: &mut NativeCtx<'_>,
    n: i64,
) -> Result<crate::object::Gc<crate::object::Table>, LuaError> {
    /// Most slots reserved up front; bigger arrays grow as they fill.
    const MAX_RESERVE: usize = 1 << 24;
    let n = (n as usize).min(MAX_RESERVE);
    ctx.reserve(n.saturating_mul(9))?;
    let Ok(t) = crate::object::Table::try_with_capacity(n, 0) else {
        return Err(ctx.no_memory());
    };
    let t = ctx.new_table(t);
    ctx.push(Value::Table(t));
    Ok(t)
}

fn at(ctx: &NativeCtx<'_>, a: crate::object::Gc<crate::object::Table>, i: i64) -> Value {
    ctx.vm.heap.get(a).get_int(i)
}

fn put(
    ctx: &mut NativeCtx<'_>,
    a: crate::object::Gc<crate::object::Table>,
    i: i64,
    v: Value,
) -> Result<(), LuaError> {
    crate::object::table::set_int(&mut ctx.vm.heap, a, i, v).map_err(|_| ctx.no_memory())?;
    Ok(())
}

/// `a < b` by the comparator `lt`, or by `<` when it is nil.
fn less(ctx: &mut NativeCtx<'_>, lt: Value, a: Value, b: Value) -> Result<bool, LuaError> {
    if lt.is_nil() {
        return ctx.less_than_now(a, b);
    }
    let at = ctx.len();
    ctx.push(lt);
    ctx.push(a);
    ctx.push(b);
    ctx.call_want(2, 1)?;
    let r = ctx.arg(at);
    ctx.truncate(at);
    Ok(!r.is_falsy())
}

/// Store the sorted copy back into `t`.
fn write_back(
    ctx: &mut NativeCtx<'_>,
    t: Value,
    plain: Option<crate::object::Gc<crate::object::Table>>,
    src: crate::object::Gc<crate::object::Table>,
    n: i64,
) -> R {
    for i in 1..=n {
        let v = at(ctx, src, i);
        match plain {
            Some(h) => put(ctx, h, i, v)?,
            None => ctx.set_index_now(t, Value::Int(i), v)?,
        }
    }
    Ok(NativeReturn::Return(0))
}

/// `table.remove`, on a plain table or through metamethods.
pub(super) fn remove(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.arg(0);
    let plain = match t {
        Value::Table(h) if ctx.vm.heap.get(h).meta.is_none() => Some(h),
        _ => None,
    };
    let size = match plain {
        Some(h) => ctx.vm.heap.get(h).border(),
        None => {
            let t = check_tab(ctx, 0, READ_WRITE)?;
            length(ctx, t)?
        }
    };
    let mut pos = size;
    if !ctx.arg(1).is_nil() {
        pos = ctx.check_int(1)?;
        // `pos - 1` as unsigned: 1..=size+1 is valid.
        if pos != size && (size as u64) < (pos.wrapping_sub(1) as u64) {
            return Err(ctx.arg_error(1, "position out of bounds"));
        }
    }
    window(ctx, 2);
    let v = get_at(ctx, t, plain, pos)?;
    // Rooted while the moves below run metamethods.
    ctx.set(1, v);
    while pos < size {
        let next = get_at(ctx, t, plain, pos + 1)?;
        set_at(ctx, t, plain, pos, next)?;
        pos += 1;
    }
    set_at(ctx, t, plain, pos, Value::Nil)?;
    let v = ctx.arg(1);
    ctx.ret([v])
}

/// `t[i]`, raw when `plain` is `t` itself.
fn get_at(
    ctx: &mut NativeCtx<'_>,
    t: Value,
    plain: Option<crate::object::Gc<crate::object::Table>>,
    i: i64,
) -> Result<Value, LuaError> {
    match plain {
        Some(h) => Ok(at(ctx, h, i)),
        None => ctx.index_now(t, Value::Int(i)),
    }
}

/// `t[i] = v`, raw when `plain` is `t` itself.
fn set_at(
    ctx: &mut NativeCtx<'_>,
    t: Value,
    plain: Option<crate::object::Gc<crate::object::Table>>,
    i: i64,
    v: Value,
) -> Result<(), LuaError> {
    match plain {
        Some(h) => put(ctx, h, i, v),
        None => ctx.set_index_now(t, Value::Int(i), v),
    }
}

/// `sort`'s fast path, for no comparator: sort `a[1..=n]` by `<` if they are all numbers
/// (none NaN) or all strings in byte order, and return true; otherwise
/// leave `a` alone and return false.
fn sort_primitive(ctx: &mut NativeCtx<'_>, a: Gc<Table>, n: i64) -> Result<bool, LuaError> {
    let heap = &ctx.vm.heap;
    let tab = heap.get(a);
    let values: Vec<Value> = (1..=n).map(|i| tab.get_int(i)).collect();
    let numbers = values.iter().all(|v| match *v {
        Value::Int(_) => true,
        Value::Float(f) => !f.is_nan(),
        _ => false,
    });
    // Strings in byte order only: a locale's collation goes through `<`
    // with the built-in's own sort.
    let strings = !numbers
        && ctx.vm.rt.metas.locale.byte_order()
        && values.iter().all(|v| matches!(v, Value::Str(_)));
    if !numbers && !strings {
        return Ok(false);
    }
    let mut values = values;
    values.sort_by(|&x, &y| {
        let lt = |x, y| sort_lt(ctx.vm, x, y);
        if lt(x, y) {
            std::cmp::Ordering::Less
        } else if lt(y, x) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    for (i, v) in values.into_iter().enumerate() {
        crate::object::table::set_int(&mut ctx.vm.heap, a, i as i64 + 1, v)
            .map_err(|_| ctx.no_memory())?;
    }
    Ok(true)
}

/// `<` for [`sort_primitive`], whose strings compare in byte order.
fn sort_lt(vm: &Vm, x: Value, y: Value) -> bool {
    match (x, y) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::Str(a), Value::Str(b)) => vm.heap.get(a).as_bytes() < vm.heap.get(b).as_bytes(),
        _ => crate::vm::ops::less_than(&vm.heap, &vm.rt.metas, x, y).unwrap_or(false),
    }
}
