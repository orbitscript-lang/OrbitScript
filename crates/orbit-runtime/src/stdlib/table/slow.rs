//! The table functions on values with metatables: every read, write and
//! length goes through `__index`, `__newindex` and `__len`, in the order
//! lua5.5 makes them (found by logging metamethod calls on proxies).
//!
//! Metamethods run nested ([`NativeCtx::call_want`]), so each function is a
//! plain loop; as in the reference, nothing may yield across them.

use crate::object::{Gc, HashSeed, Table, Value};
use crate::stack::{Buffer, CompareOp, Return, Stack, StackResult, Type};
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

/// Argument `arg` is a table, or has the metamethods `needs` names.
fn check_table(s: &mut Stack<'_>, arg: i32, needs: Needs) -> StackResult<()> {
    if s.type_at(arg) == Type::Table {
        return Ok(());
    }
    let ok = s.get_metatable(arg)
        && (!needs.read || s.has_meta_field(arg, MetaField::Index))
        && (!needs.write || s.has_meta_field(arg, MetaField::NewIndex));
    if ok {
        s.set_top(-2)?;
        Ok(())
    } else {
        Err(s.type_error(arg, "table"))
    }
}

/// `#t` with `__len` for the value at `idx`, as the table functions take
/// it: an integer, a float with an integral value, or a string that
/// converts to one.
fn table_length(s: &mut Stack<'_>, idx: i32) -> StackResult<i64> {
    s.len(idx)?;
    let n = s.to_integer(-1);
    s.set_top(-2)?;
    match n {
        Some(n) => Ok(n),
        None => Err(s.error("object length is not an integer")),
    }
}

pub(super) fn concat(s: &mut Stack<'_>) -> StackResult<Buffer> {
    check_table(s, 1, READ)?;
    s.set_top(4)?;
    // The length comes first, even when the range is given.
    let last = table_length(s, 1)?;
    let sep = if s.type_at(2) == Type::Nil {
        Vec::new()
    } else {
        let sep = s.check_string_handle(2)?;
        s.handle_bytes(sep).to_vec()
    };
    let mut i = s.opt_integer(3, 1)?;
    let last = s.opt_integer(4, last)?;
    let mut out = Vec::new();
    while i <= last {
        s.get_index(1, i)?;
        if let Err(type_name) = s.append_text_at(-1, sep.len(), &mut out)? {
            return Err(super::invalid_value(s, type_name, i));
        }
        s.set_top(4)?;
        if i == last {
            break;
        }
        out.extend_from_slice(&sep);
        i += 1;
    }
    Ok(Buffer(out))
}

pub(super) fn unpack(s: &mut Stack<'_>) -> StackResult<Return> {
    check_table(s, 1, READ)?;
    s.set_top(3)?;
    let len = table_length(s, 1)?;
    let i = s.opt_integer(2, 1)?;
    let j = s.opt_integer(3, len)?;
    if i > j {
        return Ok(Return::values(0));
    }
    let room = s.room();
    let Some(n) = j
        .checked_sub(i)
        .and_then(|d| d.checked_add(1))
        // More results than an integer holds, or the stack.
        .filter(|&n| n < MAX_INT && (n as u64) < room as u64)
    else {
        return Err(s.error("too many results to unpack"));
    };
    // Each value goes on the window as it arrives, rooted while the next
    // metamethods run.
    let mut k = i;
    loop {
        s.get_index(1, k)?;
        if k == j {
            break;
        }
        k += 1;
    }
    Ok(Return::values(n as usize))
}

pub(super) fn move_(s: &mut Stack<'_>) -> StackResult<Return> {
    check_table(s, 1, READ)?;
    let f = s.check_integer(2)?;
    let e = s.check_integer(3)?;
    let t = s.check_integer(4)?;
    let explicit = !matches!(s.type_at(5), Type::None | Type::Nil);
    if explicit {
        check_table(s, 5, WRITE)?;
    }
    s.set_top(5)?;
    if !explicit {
        s.copy(1, 5)?;
    }
    if e < f {
        s.push_copy(5);
        return Ok(Return::values(1));
    }
    // The elements after the first; their count must fit an integer.
    let Some(n) = e.checked_sub(f).filter(|&n| n < i64::MAX) else {
        return Err(s.arg_error(3, "too many elements to move"));
    };
    if t.checked_add(n).is_none() {
        return Err(s.arg_error(4, "destination wrap around"));
    }
    // Moving up is safe unless the destination starts inside the source of
    // the same table (`__eq` included when the tables were given apart).
    let down = !(t > e || t <= f) && (!explicit || s.compare(1, 5, CompareOp::Eq)?);
    let step = |s: &mut Stack<'_>, i: i64| -> StackResult<()> {
        s.get_index(1, f + i)?;
        s.set_index_int(5, t + i)
    };
    if down {
        for i in (0..=n).rev() {
            step(s, i)?;
        }
    } else {
        for i in 0..=n {
            step(s, i)?;
        }
    }
    s.push_copy(5);
    Ok(Return::values(1))
}

pub(super) fn insert(s: &mut Stack<'_>) -> StackResult<Return> {
    check_table(s, 1, READ_WRITE)?;
    let nargs = s.top();
    let e = table_length(s, 1)?.wrapping_add(1);
    match nargs {
        2 => {
            s.push_copy(2);
            s.set_index_int(1, e)?;
        }
        3 => {
            let pos = s.check_integer(2)?;
            // `pos - 1` as unsigned: 1..=e is valid.
            if (pos.wrapping_sub(1) as u64) >= e as u64 {
                return Err(s.arg_error(2, "position out of bounds"));
            }
            let mut i = e;
            while i > pos {
                s.get_index(1, i - 1)?;
                s.set_index_int(1, i)?;
                i -= 1;
            }
            s.push_copy(3);
            s.set_index_int(1, pos)?;
        }
        _ => return Err(s.error("wrong number of arguments to 'insert'")),
    }
    Ok(Return::values(0))
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
        Some(h) => ctx.vm.heap.get(h).border(HashSeed::of(&ctx.vm.heap)),
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
            Some(h) => ctx.vm.heap.get(h).get_int(HashSeed::of(&ctx.vm.heap), i),
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
    ctx.vm.heap.get(a).get_int(HashSeed::of(&ctx.vm.heap), i)
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
pub(super) fn remove(s: &mut Stack<'_>) -> StackResult<Return> {
    let plain = s.plain_table(1);
    let size = match plain {
        Some(t) => s.raw_border(t),
        None => {
            check_table(s, 1, READ_WRITE)?;
            table_length(s, 1)?
        }
    };
    let mut pos = size;
    if !matches!(s.type_at(2), Type::None | Type::Nil) {
        pos = s.check_integer(2)?;
        // `pos - 1` as unsigned: 1..=size+1 is valid.
        if pos != size && (size as u64) < (pos.wrapping_sub(1) as u64) {
            return Err(s.arg_error(2, "position out of bounds"));
        }
    }
    s.set_top(1)?;
    // The removed value, slot 2, stays rooted while the moves below run
    // metamethods.
    match plain {
        Some(t) => s.push_raw_index(t, pos),
        None => {
            s.get_index(1, pos)?;
        }
    }
    while pos < size {
        match plain {
            Some(t) => s.raw_copy_int(t, pos + 1, t, pos)?,
            None => {
                s.get_index(1, pos + 1)?;
                s.set_index_int(1, pos)?;
            }
        }
        pos += 1;
    }
    s.push_nil();
    match plain {
        Some(t) => s.raw_set_int_from(t, pos, -1)?,
        None => s.set_index_int(1, pos)?,
    }
    if plain.is_some() {
        s.set_top(2)?;
    }
    Ok(Return::values(1))
}

/// `sort`'s fast path, for no comparator: sort `a[1..=n]` by `<` if they are all numbers
/// (none NaN) or all strings in byte order, and return true; otherwise
/// leave `a` alone and return false.
fn sort_primitive(ctx: &mut NativeCtx<'_>, a: Gc<Table>, n: i64) -> Result<bool, LuaError> {
    let heap = &ctx.vm.heap;
    let tab = heap.get(a);
    let seed = HashSeed::of(heap);
    let values: Vec<Value> = (1..=n).map(|i| tab.get_int(seed, i)).collect();
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
