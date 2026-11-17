//! The `table` library's functions. `orbit-std` builds the library's
//! table from them, taking each by name ([`super::library_function`]).
//!
//! Each function has a raw loop for tables without a metatable, the common
//! case, and a slow path in [`slow`] that honours `__index`, `__newindex`
//! and `__len` (docs/design/stdlib-string-table-math.md#table).

mod slow;

pub(crate) use slow::sort_primitive;

use crate::stack::{
    Buffer, Fail, LibraryRow, Rest, Results, Return, Stack, StackResult, Type, library_row,
};
use orbit_macros::native;

pub(super) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<concat>(),
    library_row::<unpack>(),
    library_row::<pack>(),
    library_row::<move_>(),
    library_row::<insert>(),
    library_row::<sort>(),
    library_row::<remove>(),
];

/// Lua 5.5's additions, absent from 5.4 builds (ADR-14).
pub(super) const FUNCTIONS_5_5: &[LibraryRow] = &[library_row::<create>()];

/// Largest array size `create` accepts (a C `int`).
const MAX_INT: i64 = i32::MAX as i64;
/// Most hash entries `create` preallocates before `table overflow`.
const MAX_HASH: i64 = 1 << 30;
/// Most slots `create` actually reserves up front; larger requests grow on
/// demand instead of failing the allocation outright.
const MAX_RESERVE: usize = 1 << 24;

/// Whether argument `arg` is absent or nil.
fn absent(s: &Stack<'_>, arg: i32) -> bool {
    matches!(s.type_at(arg), Type::None | Type::Nil)
}

#[native(crate = crate, name = "table.concat")]
fn concat(s: &mut Stack<'_>) -> StackResult<Buffer> {
    let Some(t) = s.plain_table(1) else {
        return slow::concat(s);
    };
    let sep = separator(s)?;
    let i = s.opt_integer(3, 1)?;
    let j = if absent(s, 4) {
        s.raw_border(t)
    } else {
        s.check_integer(4)?
    };
    let mut out = Vec::new();
    let mut k = i;
    while k <= j {
        if let Err(type_name) = s.append_raw_index(t, k, sep.len(), &mut out)? {
            return Err(invalid_value(s, type_name, k));
        }
        if k == j {
            break;
        }
        out.extend_from_slice(&sep);
        k += 1;
    }
    Ok(Buffer(out))
}

/// `concat`'s separator, argument 2.
fn separator(s: &mut Stack<'_>) -> StackResult<Vec<u8>> {
    if absent(s, 2) {
        return Ok(Vec::new());
    }
    let sep = s.check_string_handle(2)?;
    Ok(s.handle_bytes(sep).to_vec())
}

#[cold]
#[inline(never)]
fn invalid_value(s: &mut Stack<'_>, type_name: &str, k: i64) -> crate::stack::Raised {
    let message = format!("invalid value ({type_name}) at index {k} in table for 'concat'");
    s.error(message.as_str())
}

#[native(crate = crate, name = "table.unpack")]
fn unpack(s: &mut Stack<'_>) -> StackResult<Return> {
    let Some(t) = s.plain_table(1) else {
        return slow::unpack(s);
    };
    let i = s.opt_integer(2, 1)?;
    let j = if matches!(
        s.type_at(3),
        crate::stack::Type::None | crate::stack::Type::Nil
    ) {
        s.raw_border(t)
    } else {
        s.check_integer(3)?
    };
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
    let mut k = i;
    loop {
        s.push_raw_index(t, k);
        if k == j {
            break;
        }
        k += 1;
    }
    Ok(Return::values(n as usize))
}

#[native(crate = crate, name = "table.insert")]
fn insert(s: &mut Stack<'_>) -> StackResult<Return> {
    let Some(t) = s.plain_table(1) else {
        return slow::insert(s);
    };
    let e = s.raw_border(t).wrapping_add(1);
    match s.top() {
        2 => s.raw_set_int_from(t, e, 2)?,
        3 => {
            let pos = s.check_integer(2)?;
            // `pos - 1` as unsigned: 1..=e is valid.
            if (pos.wrapping_sub(1) as u64) >= e as u64 {
                return Err(s.arg_error(2, "position out of bounds"));
            }
            let mut i = e;
            while i > pos {
                s.raw_copy_int(t, i - 1, t, i)?;
                i -= 1;
            }
            s.raw_set_int_from(t, pos, 3)?;
        }
        _ => return Err(s.error("wrong number of arguments to 'insert'")),
    }
    Ok(Return::values(0))
}

#[native(crate = crate, name = "table.sort")]
fn sort(s: &mut Stack<'_>) -> StackResult<Return> {
    slow::sort(s)
}

#[native(crate = crate, name = "table.remove")]
fn remove(s: &mut Stack<'_>) -> StackResult<Return> {
    slow::remove(s)
}

#[native(crate = crate, name = "table.pack")]
fn pack(s: &mut Stack<'_>, values: Rest) -> StackResult<Return> {
    s.push_array_of(values.first, values.count, 1)?;
    s.push_integer(values.count as i64);
    s.raw_set_field(-2, "n")?;
    Ok(Return::values(1))
}

#[native(crate = crate, name = "table.move")]
fn move_(s: &mut Stack<'_>) -> StackResult<Return> {
    let explicit = !absent(s, 5);
    let (Some(a1), true) = (s.plain_table(1), !explicit || s.plain_table(5).is_some()) else {
        return slow::move_(s);
    };
    let f = s.check_integer(2)?;
    let e = s.check_integer(3)?;
    let t = s.check_integer(4)?;
    let a2 = s.plain_table(5).unwrap_or(a1);
    if e >= f {
        // The elements after the first; their count must fit an integer.
        let Some(n) = e.checked_sub(f).filter(|&n| n < i64::MAX) else {
            return Err(s.arg_error(3, "too many elements to move"));
        };
        if t.checked_add(n).is_none() {
            return Err(s.arg_error(4, "destination wrap around"));
        }
        let overlapping = a1 == a2 && t > f && t <= e;
        if overlapping {
            for i in (0..=n).rev() {
                s.raw_copy_int(a1, f + i, a2, t + i)?;
            }
        } else {
            for i in 0..=n {
                s.raw_copy_int(a1, f + i, a2, t + i)?;
            }
        }
    }
    s.push_copy(if explicit { 5 } else { 1 });
    Ok(Return::values(1))
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
