//! The table library (`ltablib.c`). Element access goes through metamethods, as with
//! `lua_geti`/`lua_seti`.

use orbit_core::Value;
use orbit_vm::{Action, LuaResult, Vm};

/// Opens the table library.
pub fn open_table(vm: &mut Vm) {
    vm.new_lib(
        "table",
        &[
            ("concat", tconcat),
            ("create", tcreate),
            ("insert", tinsert),
            ("pack", tpack),
            ("unpack", tunpack),
            ("remove", tremove),
            ("move", tmove),
            ("sort", sort),
        ],
    );
}

const TAB_R: u8 = 1;
const TAB_W: u8 = 2;
const TAB_L: u8 = 4;
const TAB_RW: u8 = TAB_R | TAB_W;

/// `checktab`: argument `arg` is a table, or has the metamethods to act like one.
fn checktab(vm: &mut Vm, arg: usize, what: u8) -> LuaResult<()> {
    let v = vm.arg(arg);
    if matches!(v, Value::Table(_)) {
        return Ok(());
    }
    let ok = vm.heap.metatable(v).is_some()
        && (what & TAB_R == 0 || !vm.meta_field(v, b"__index").is_nil())
        && (what & TAB_W == 0 || !vm.meta_field(v, b"__newindex").is_nil())
        && (what & TAB_L == 0
            || matches!(v, Value::Str(_))
            || !vm.meta_field(v, b"__len").is_nil());
    if ok {
        Ok(())
    } else {
        Err(vm.type_arg_error(arg, "table"))
    }
}

/// `aux_getn`
fn getn(vm: &mut Vm, arg: usize, what: u8) -> LuaResult<i64> {
    checktab(vm, arg, what | TAB_L)?;
    let v = vm.arg(arg);
    vm.length_int(v)
}

fn tcreate(vm: &mut Vm) -> LuaResult<Action> {
    let seq = vm.check_integer(1)? as u64;
    let rest = vm.opt_integer(2, 0)? as u64;
    if seq > i32::MAX as u64 {
        return Err(vm.arg_error(1, "out of range"));
    }
    if rest > i32::MAX as u64 {
        return Err(vm.arg_error(2, "out of range"));
    }
    let t = match vm.heap.new_table_with(seq as u32, rest as u32) {
        Ok(t) => t,
        Err(orbit_core::table::SetError::Overflow) => return Err(vm.rt_error("table overflow")),
        Err(_) => return Err(vm.error("not enough memory")),
    };
    vm.push(Value::Table(t));
    Ok(Action::Return(1))
}

fn tinsert(vm: &mut Vm) -> LuaResult<Action> {
    let e = getn(vm, 1, TAB_RW)?.wrapping_add(1); // first empty element
    let t = vm.arg(1);
    let pos = match vm.top() {
        2 => e,
        3 => {
            let pos = vm.check_integer(2)?;
            if (pos as u64).wrapping_sub(1) >= e as u64 {
                return Err(vm.arg_error(2, "position out of bounds"));
            }
            let mut i = e;
            while i > pos {
                let v = vm.get_index(t, i - 1)?;
                vm.set_index(t, i, v)?;
                i -= 1;
            }
            pos
        }
        _ => return Err(vm.error("wrong number of arguments to 'insert'")),
    };
    let v = vm.arg(vm.top());
    vm.set_index(t, pos, v)?;
    Ok(Action::Return(0))
}

fn tremove(vm: &mut Vm) -> LuaResult<Action> {
    let size = getn(vm, 1, TAB_RW)?;
    let t = vm.arg(1);
    let mut pos = vm.opt_integer(2, size)?;
    if pos != size && (pos as u64).wrapping_sub(1) > size as u64 {
        return Err(vm.arg_error(2, "position out of bounds"));
    }
    let result = vm.get_index(t, pos)?;
    while pos < size {
        let v = vm.get_index(t, pos + 1)?;
        vm.set_index(t, pos, v)?;
        pos += 1;
    }
    vm.set_index(t, pos, Value::Nil)?;
    vm.push(result);
    Ok(Action::Return(1))
}

fn tmove(vm: &mut Vm) -> LuaResult<Action> {
    let f = vm.check_integer(2)?;
    let e = vm.check_integer(3)?;
    let t = vm.check_integer(4)?;
    let tt = if vm.arg(5).is_nil() { 1 } else { 5 };
    checktab(vm, 1, TAB_R)?;
    checktab(vm, tt, TAB_W)?;
    let (src, dst) = (vm.arg(1), vm.arg(tt));
    if e >= f {
        if !(f > 0 || e < i64::MAX.wrapping_add(f)) {
            return Err(vm.arg_error(3, "too many elements to move"));
        }
        let n = e - f + 1;
        if t > i64::MAX - n + 1 {
            return Err(vm.arg_error(4, "destination wrap around"));
        }
        let other = tt != 1 && !vm.equals(src, dst)?;
        if t > e || t <= f || other {
            for i in 0..n {
                let v = vm.get_index(src, f + i)?;
                vm.set_index(dst, t + i, v)?;
            }
        } else {
            for i in (0..n).rev() {
                let v = vm.get_index(src, f + i)?;
                vm.set_index(dst, t + i, v)?;
            }
        }
    }
    vm.push(dst);
    Ok(Action::Return(1))
}

fn add_field(vm: &mut Vm, t: Value, i: i64, out: &mut Vec<u8>) -> LuaResult<()> {
    let v = vm.get_index(t, i)?;
    match v {
        Value::Str(s) => out.extend_from_slice(vm.heap.string(s).as_bytes()),
        Value::Int(_) | Value::Float(_) => {
            out.extend_from_slice(orbit_vm::number::fmt_number(v).as_bytes())
        }
        _ => {
            let tn = vm.type_name_of(v);
            return Err(vm.error(format!(
                "invalid value ({tn}) at index {i} in table for 'concat'"
            )));
        }
    }
    Ok(())
}

fn tconcat(vm: &mut Vm) -> LuaResult<Action> {
    let mut last = getn(vm, 1, TAB_R)?;
    let t = vm.arg(1);
    let sep = match vm.opt_string(2)? {
        Some(s) => vm.heap.string(s).as_bytes().to_vec(),
        None => Vec::new(),
    };
    let mut i = vm.opt_integer(3, 1)?;
    last = vm.opt_integer(4, last)?;
    let mut out = Vec::new();
    while i < last {
        add_field(vm, t, i, &mut out)?;
        out.extend_from_slice(&sep);
        i += 1;
    }
    if i == last {
        add_field(vm, t, i, &mut out)?;
    }
    let v = vm.heap.str_value(out);
    vm.push(v);
    Ok(Action::Return(1))
}

fn tpack(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    let t = match vm.heap.new_table_with(n as u32, 1) {
        Ok(t) => t,
        Err(orbit_core::table::SetError::Overflow) => return Err(vm.rt_error("table overflow")),
        Err(_) => return Err(vm.error("not enough memory")),
    };
    for i in 1..=n {
        let v = vm.arg(i);
        vm.heap.raw_set_int(t, i as i64, v).expect("integer key");
    }
    vm.heap
        .raw_set_name(t, b"n", Value::Int(n as i64))
        .expect("string key");
    vm.push(Value::Table(t));
    Ok(Action::Return(1))
}

fn tunpack(vm: &mut Vm) -> LuaResult<Action> {
    let t = vm.arg(1);
    let mut i = vm.opt_integer(2, 1)?;
    let e = if vm.arg(3).is_nil() {
        vm.length_int(t)?
    } else {
        vm.check_integer(3)?
    };
    if i > e {
        return Ok(Action::Return(0));
    }
    let n = (e as u64).wrapping_sub(i as u64);
    if n >= i32::MAX as u64 || vm.check_stack(n as usize + 1).is_err() {
        return Err(vm.error("too many results to unpack"));
    }
    while i < e {
        let v = vm.get_index(t, i)?;
        vm.push(v);
        i += 1;
    }
    let v = vm.get_index(t, e)?;
    vm.push(v);
    Ok(Action::Return(n as usize + 1))
}

// ---- sort ----------------------------------------------------------------------

/// Arrays larger than this may use randomized pivots (`RANLIMIT`).
const RANLIMIT: u32 = 100;

struct Sorter {
    t: Value,
    cmp: Value,
}

impl Sorter {
    fn get(&self, vm: &mut Vm, i: u32) -> LuaResult<Value> {
        vm.get_index(self.t, i64::from(i))
    }

    fn set(&self, vm: &mut Vm, i: u32, v: Value) -> LuaResult<()> {
        vm.set_index(self.t, i64::from(i), v)
    }

    /// `sort_comp`: `a < b` in the sort's order.
    fn lt(&self, vm: &mut Vm, a: Value, b: Value) -> LuaResult<bool> {
        if self.cmp.is_nil() {
            vm.less_than(a, b)
        } else {
            Ok(vm.call_value(self.cmp, &[a, b])?.is_truthy())
        }
    }

    fn order_error(vm: &mut Vm) -> orbit_vm::Error {
        vm.error("invalid order function for sorting")
    }

    /// `partition`, with the pivot `p` equal to `a[up - 1]`.
    fn partition(&self, vm: &mut Vm, lo: u32, up: u32, p: Value) -> LuaResult<u32> {
        let mut i = lo;
        let mut j = up - 1;
        loop {
            // repeat ++i while a[i] < P
            let mut ai;
            loop {
                i += 1;
                ai = self.get(vm, i)?;
                if !self.lt(vm, ai, p)? {
                    break;
                }
                if i == up - 1 {
                    return Err(Self::order_error(vm));
                }
            }
            // repeat --j while P < a[j]
            let mut aj;
            loop {
                j -= 1;
                aj = self.get(vm, j)?;
                if !self.lt(vm, p, aj)? {
                    break;
                }
                if j < i {
                    return Err(Self::order_error(vm));
                }
            }
            if j < i {
                // swap the pivot (a[up - 1]) with a[i]
                self.set(vm, up - 1, ai)?;
                self.set(vm, i, p)?;
                return Ok(i);
            }
            self.set(vm, i, aj)?;
            self.set(vm, j, ai)?;
        }
    }

    fn auxsort(&self, vm: &mut Vm, mut lo: u32, mut up: u32, mut rnd: u32) -> LuaResult<()> {
        while lo < up {
            let alo = self.get(vm, lo)?;
            let aup = self.get(vm, up)?;
            if self.lt(vm, aup, alo)? {
                self.set(vm, lo, aup)?;
                self.set(vm, up, alo)?;
            }
            if up - lo == 1 {
                return Ok(());
            }
            let mut p = if up - lo < RANLIMIT || rnd == 0 {
                lo + (up - lo) / 2
            } else {
                let r4 = (up - lo) / 4;
                (rnd ^ lo ^ up) % (r4 * 2) + (lo + r4)
            };
            let ap = self.get(vm, p)?;
            let alo = self.get(vm, lo)?;
            if self.lt(vm, ap, alo)? {
                self.set(vm, p, alo)?;
                self.set(vm, lo, ap)?;
            } else {
                let aup = self.get(vm, up)?;
                if self.lt(vm, aup, ap)? {
                    self.set(vm, p, aup)?;
                    self.set(vm, up, ap)?;
                }
            }
            if up - lo == 2 {
                return Ok(());
            }
            let pivot = self.get(vm, p)?;
            let aup1 = self.get(vm, up - 1)?;
            self.set(vm, p, aup1)?;
            self.set(vm, up - 1, pivot)?;
            p = self.partition(vm, lo, up, pivot)?;
            let n;
            if p - lo < up - p {
                self.auxsort(vm, lo, p - 1, rnd)?;
                n = p - lo;
                lo = p + 1;
            } else {
                self.auxsort(vm, p + 1, up, rnd)?;
                n = up - p;
                up = p - 1;
            }
            if up.wrapping_sub(lo) / 128 > n {
                rnd = random_seed();
            }
        }
        Ok(())
    }
}

/// `l_randomizePivot`: some varying bits.
fn random_seed() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() ^ d.as_secs() as u32);
    t | 1
}

fn sort(vm: &mut Vm) -> LuaResult<Action> {
    let n = getn(vm, 1, TAB_RW)?;
    if n > 1 {
        if n >= i64::from(i32::MAX) {
            return Err(vm.arg_error(1, "array too big"));
        }
        let cmp = if vm.arg(2).is_nil() {
            Value::Nil
        } else {
            vm.check_function(2)?
        };
        let s = Sorter { t: vm.arg(1), cmp };
        s.auxsort(vm, 1, n as u32, 0)?;
    }
    Ok(Action::Return(0))
}
