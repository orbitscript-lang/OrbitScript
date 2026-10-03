//! The `table` library subset (raw access: metamethods come later).

use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("table.insert", insert),
    ("table.remove", remove),
    ("table.concat", concat),
    ("table.unpack", unpack),
];

fn get(ctx: &NativeCtx<'_>, t: Gc<Table>, i: i64) -> Value {
    ctx.heap.get(t).get_int(i)
}

fn set(ctx: &mut NativeCtx<'_>, t: Gc<Table>, i: i64, v: Value) {
    ctx.heap.update(t, |tab| tab.set_int(i, v));
}

fn insert(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let n = ctx.heap.get(t).border();
    match ctx.nargs() {
        2 => {
            let v = ctx.arg(1);
            set(ctx, t, n.wrapping_add(1), v);
        }
        3 => {
            let pos = ctx.check_int(1)?;
            // `pos - 1` as unsigned: 1..=n+1 is valid.
            if (pos as u64).wrapping_sub(1) >= (n as u64).wrapping_add(1) {
                return Err(ctx.arg_error(1, "position out of bounds"));
            }
            let mut i = n + 1;
            while i > pos {
                let v = get(ctx, t, i - 1);
                set(ctx, t, i, v);
                i -= 1;
            }
            let v = ctx.arg(2);
            set(ctx, t, pos, v);
        }
        _ => return Err(ctx.error("wrong number of arguments to 'insert'")),
    }
    Ok(NativeReturn::Return(0))
}

fn remove(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let size = ctx.heap.get(t).border();
    let mut pos = ctx.opt_int(1, size)?;
    if ctx.nargs() > 1 && size.wrapping_add(1) as u64 != pos as u64 {
        // `pos - 1` as unsigned: 1..=size+1 is valid.
        if (pos as u64).wrapping_sub(1) >= (size as u64).wrapping_add(1) {
            return Err(ctx.arg_error(1, "position out of bounds"));
        }
    }
    let v = get(ctx, t, pos);
    while pos < size {
        let next = get(ctx, t, pos + 1);
        set(ctx, t, pos, next);
        pos += 1;
    }
    if pos <= size || ctx.nargs() > 1 {
        set(ctx, t, pos, Value::Nil);
    }
    ctx.ret1(v)
}

fn concat(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let sep = if ctx.arg(1).is_nil() {
        Vec::new()
    } else {
        let s = ctx.check_str(1)?;
        ctx.bytes(s).to_vec()
    };
    let i = ctx.opt_int(2, 1)?;
    let j = if ctx.arg(3).is_nil() {
        ctx.heap.get(t).border()
    } else {
        ctx.check_int(3)?
    };
    let mut out = Vec::new();
    let mut k = i;
    while k <= j {
        let v = get(ctx, t, k);
        match v {
            Value::Str(_) | Value::Int(_) | Value::Float(_) => ctx.display(v, &mut out),
            _ => {
                return Err(ctx.error(format!(
                    "invalid value ({}) at index {k} in table for 'concat'",
                    v.type_name()
                )));
            }
        }
        if k != j {
            out.extend_from_slice(&sep);
        }
        if k == i64::MAX {
            break;
        }
        k += 1;
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

fn unpack(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let i = ctx.opt_int(1, 1)?;
    let j = if ctx.arg(2).is_nil() {
        ctx.heap.get(t).border()
    } else {
        ctx.check_int(2)?
    };
    if i > j {
        return Ok(NativeReturn::Return(0));
    }
    let n = (j as i128 - i as i128 + 1) as u128;
    if n >= 1_000_000 {
        return Err(ctx.error("too many results to unpack"));
    }
    for k in i..=j {
        let v = get(ctx, t, k);
        ctx.push(v);
    }
    Ok(NativeReturn::Return(n as usize))
}
