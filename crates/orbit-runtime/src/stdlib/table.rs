//! The `table` library.
//!
//! Each function has a raw loop for tables without a metatable, the common
//! case, and a slow path in [`slow`] that honours `__index`, `__newindex`
//! and `__len` (docs/design/stdlib-string-table-math.md#table).

mod slow;

use crate::object::table as tbl;
use crate::object::{Gc, Table, Value};
use crate::stack::{Fail, LibraryRow, Rest, Results, Return, Stack, StackResult, library_row};
use crate::vm::{LuaError, NativeCtx, NativeReturn};
use orbit_macros::native;

type R = Result<NativeReturn, LuaError>;

// `concat`, `unpack` and `move` run metamethods on their slow paths, so
// they stay on the raw interface until the language rule's step gives them
// a Rust fast path and an OrbitScript slow path.
pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    LibraryRow::raw("table.concat", concat),
    LibraryRow::raw("table.unpack", unpack),
    library_row::<pack>(),
    LibraryRow::raw("table.move", move_),
    LibraryRow::raw("table.insert", insert),
    LibraryRow::raw("table.sort", slow::sort),
    LibraryRow::raw("table.remove", slow::remove),
];

/// Lua 5.5's additions, absent from 5.4 builds (ADR-14).
pub(crate) const FUNCTIONS_5_5: &[LibraryRow] = &[library_row::<create>()];

/// Largest array size `create` accepts (a C `int`).
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
                return Err(ctx.error(&format!(
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
    ctx.ret([r])
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
    let room = ctx.vm.rt.host.max_stack().saturating_sub(ctx.len());
    let Some(n) = j
        .checked_sub(i)
        .and_then(|d| d.checked_add(1))
        // More results than an integer holds, or the stack.
        .filter(|&n| n < MAX_INT && (n as u64) < room as u64)
    else {
        return Err(ctx.error("too many results to unpack"));
    };
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

fn insert(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        return slow::insert(ctx);
    };
    let e = length(ctx, t).wrapping_add(1);
    match ctx.nargs() {
        2 => {
            let v = ctx.arg(1);
            set(ctx, t, e, v)?;
        }
        3 => {
            let pos = ctx.check_int(1)?;
            // `pos - 1` as unsigned: 1..=e is valid.
            if (pos.wrapping_sub(1) as u64) >= e as u64 {
                return Err(ctx.arg_error(1, "position out of bounds"));
            }
            let mut i = e;
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

#[native(crate = crate, name = "table.pack")]
fn pack(s: &mut Stack<'_>, values: Rest) -> StackResult<Return> {
    s.push_array_of(values.first, values.count, 1)?;
    s.push_integer(values.count as i64);
    s.raw_set_field(-2, "n")?;
    Ok(Return::values(1))
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
        // The elements after the first; their count must fit an integer.
        let Some(n) = e.checked_sub(f).filter(|&n| n < i64::MAX) else {
            return Err(ctx.arg_error(2, "too many elements to move"));
        };
        if t.checked_add(n).is_none() {
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
    ctx.ret([Value::Table(a2)])
}

// Both arguments are checked before either's range, as the reference does.
#[native(crate = crate, name = "table.create")]
fn create(n: i64, m: Option<i64>) -> Result<NewTable, Fail> {
    if !(0..=MAX_INT).contains(&n) {
        return Err(Fail::arg(1, "out of range"));
    }
    let m = m.unwrap_or(0);
    if !(0..=MAX_INT).contains(&m) {
        return Err(Fail::arg(2, "out of range"));
    }
    if m > MAX_HASH {
        return Err("table overflow".into());
    }
    Ok(NewTable {
        narr: (n as usize).min(MAX_RESERVE),
        nrec: (m as usize).min(MAX_RESERVE),
    })
}

/// `create`'s result: a table with room for `narr` array and `nrec` hash
/// entries.
struct NewTable {
    narr: usize,
    nrec: usize,
}

impl Results for NewTable {
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        // About what the parts take: 9 bytes an array slot, a hash entry
        // plus load-factor slack.
        let bytes = self
            .narr
            .saturating_mul(9)
            .saturating_add(self.nrec.saturating_mul(32));
        s.reserve_memory(bytes)?;
        s.try_create_table(self.narr, self.nrec)?;
        Ok(Return::values(1))
    }
}
