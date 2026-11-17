//! The table functions on values with metatables: every read, write and
//! length goes through `__index`, `__newindex` and `__len`, in the order
//! lua5.5 makes them (found by logging metamethod calls on proxies).
//!
//! Metamethods run nested (the stack interface calls them in place), so
//! each function is a plain loop; as in the reference, nothing may yield
//! across them.

use crate::object::{Gc, HashSeed, NoMemory, Table, Value};
use crate::stack::{Buffer, CompareOp, Return, Stack, StackResult, Type};
use crate::vm::{MetaField, Vm};

use super::MAX_INT;

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
#[inline]
fn table_length(s: &mut Stack<'_>, idx: i32) -> StackResult<i64> {
    match s.length_integer(idx)? {
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
        if let Err(type_name) = s.append_index(1, i, sep.len(), &mut out)? {
            return Err(super::invalid_value(s, type_name, i));
        }
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
        2 => s.set_index_int_from(1, e, 2)?,
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
            s.set_index_int_from(1, pos, 3)?;
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
/// window (slots 3 and 4), where the collector sees them while
/// comparisons run.
pub(super) fn sort(s: &mut Stack<'_>) -> StackResult<Return> {
    let plain = s.plain_table(1);
    let n = match plain {
        Some(t) => s.raw_border(t),
        None => {
            check_table(s, 1, READ_WRITE)?;
            table_length(s, 1)?
        }
    };
    if n <= 1 {
        return Ok(Return::values(0));
    }
    if n >= MAX_SORT {
        return Err(s.arg_error(1, "array too big"));
    }
    let by_function = match s.type_at(2) {
        Type::None | Type::Nil => false,
        Type::Function => true,
        _ => return Err(s.type_error(2, "function")),
    };
    s.set_top(2)?;
    let mut src = s.push_array_table(n)?;
    let mut dst = s.push_array_table(n)?;
    for i in 1..=n {
        match plain {
            Some(t) => s.raw_copy_int(t, i, src, i)?,
            None => {
                s.get_index(1, i)?;
                s.raw_set_int_from(src, i, -1)?;
                s.set_top(4)?;
            }
        }
    }
    if !by_function && s.sort_primitive(src, n)? {
        return write_back(s, plain, src, n);
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
                let right = i >= mid || (j < hi && less(s, by_function, src, j, i)?);
                let from = if right {
                    j += 1;
                    j - 1
                } else {
                    i += 1;
                    i - 1
                };
                s.raw_copy_int(src, from, dst, k)?;
            }
            lo = hi;
        }
        std::mem::swap(&mut src, &mut dst);
        width *= 2;
    }
    // A comparator or `__lt` must be a strict order: no neighbour may come
    // out smaller than the one before it.
    for i in 2..=n {
        if less(s, by_function, src, i, i - 1)? {
            return Err(s.error("invalid order function for sorting"));
        }
    }
    write_back(s, plain, src, n)
}

/// `a[i] < a[j]` by the comparator (argument 2), or by `<`.
#[inline]
fn less(s: &mut Stack<'_>, by_function: bool, a: Gc<Table>, i: i64, j: i64) -> StackResult<bool> {
    if by_function {
        s.push_copy(2);
        s.push_raw_index(a, i);
        s.push_raw_index(a, j);
        s.call(2, 1)?;
    } else {
        s.push_raw_index(a, i);
        s.push_raw_index(a, j);
        let r = s.compare(-2, -1, CompareOp::Lt)?;
        s.set_top(4)?;
        return Ok(r);
    }
    let r = s.to_boolean(-1);
    s.set_top(4)?;
    Ok(r)
}

/// Store the sorted copy back into the table (argument 1).
fn write_back(
    s: &mut Stack<'_>,
    plain: Option<Gc<Table>>,
    src: Gc<Table>,
    n: i64,
) -> StackResult<Return> {
    for i in 1..=n {
        match plain {
            Some(t) => s.raw_copy_int(src, i, t, i)?,
            None => {
                s.push_raw_index(src, i);
                s.set_index_int(1, i)?;
            }
        }
    }
    Ok(Return::values(0))
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
pub(crate) fn sort_primitive(vm: &mut Vm, a: Gc<Table>, n: i64) -> Result<bool, NoMemory> {
    let heap = &vm.heap;
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
        && vm.rt.metas.locale.byte_order()
        && values.iter().all(|v| matches!(v, Value::Str(_)));
    if !numbers && !strings {
        return Ok(false);
    }
    let mut values = values;
    values.sort_by(|&x, &y| {
        let lt = |x, y| sort_lt(vm, x, y);
        if lt(x, y) {
            std::cmp::Ordering::Less
        } else if lt(y, x) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    for (i, v) in values.into_iter().enumerate() {
        crate::object::table::set_int(&mut vm.heap, a, i as i64 + 1, v)?;
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
