//! Metamethod lookup and the slow paths of indexing, arithmetic, comparison, length
//! and concatenation (`ltm.c`, `lvm.c`).

use crate::debug::Operand;
use crate::error::LuaResult;
use crate::number::{self, Arith, F2I, fmt_number};
use crate::state::Vm;
use orbit_core::table::SetError;
use orbit_core::value::KeyError;
use orbit_core::{Tm, Value};

/// Limit on `__index`/`__newindex` chains (`MAXTAGLOOP`).
const MAX_TAG_LOOP: usize = 2000;

/// Outcome of the slow path of a table read.
pub(crate) enum Get {
    /// The value.
    Value(Value),
    /// Call `__index` function `.0` with `(.1, key)`.
    Call(Value, Value),
}

/// Outcome of the slow path of a table write.
pub(crate) enum Set {
    /// Done.
    Done,
    /// Call `__newindex` function `.0` with `(.1, key, value)`.
    Call(Value, Value),
}

impl Vm {
    /// The metamethod for `event` on `v`, or `nil` (`luaT_gettmbyobj`).
    #[inline]
    pub fn metamethod(&self, v: Value, event: Tm) -> Value {
        self.heap.tm_of(v, event)
    }

    /// The error for a failed raw set.
    pub(crate) fn set_error(&mut self, e: SetError) -> crate::Error {
        match e {
            SetError::Key(KeyError::Nil) => self.rt_error("table index is nil"),
            SetError::Key(KeyError::NaN) => self.rt_error("table index is NaN"),
            SetError::Overflow => self.rt_error("table overflow"),
        }
    }

    /// `luaV_finishget`: `t[key]` after the fast path failed.
    pub(crate) fn finish_get(&mut self, t: Value, key: Value, origin: Operand) -> LuaResult<Get> {
        let mut t = t;
        let mut origin = origin;
        for _ in 0..MAX_TAG_LOOP {
            let tm = if let Value::Table(h) = t {
                let v = self.heap.raw_get(h, key);
                if !v.is_nil() {
                    return Ok(Get::Value(v));
                }
                match self.heap.table(h).metatable() {
                    Some(mt) => self.heap.tm(mt, Tm::Index),
                    None => Value::Nil,
                }
                .pipe(|tm| if tm.is_nil() { None } else { Some(tm) })
            } else {
                let tm = self.heap.tm_of(t, Tm::Index);
                if tm.is_nil() {
                    return Err(self.index_error(t, key, origin));
                }
                Some(tm)
            };
            let Some(tm) = tm else {
                return Ok(Get::Value(Value::Nil));
            };
            if tm.is_function() {
                return Ok(Get::Call(tm, t));
            }
            t = tm;
            origin = Operand::None;
        }
        Err(self.rt_error("'__index' chain too long; possible loop"))
    }

    /// The error for indexing a value that cannot be indexed.
    fn index_error(&mut self, t: Value, _key: Value, origin: Operand) -> crate::Error {
        self.type_error(t, origin, "index")
    }

    /// `luaV_finishset`: `t[key] = val` after the fast path failed.
    pub(crate) fn finish_set(
        &mut self,
        t: Value,
        key: Value,
        val: Value,
        origin: Operand,
    ) -> LuaResult<Set> {
        let mut t = t;
        let mut origin = origin;
        for _ in 0..MAX_TAG_LOOP {
            let tm = if let Value::Table(h) = t {
                let tm = match self.heap.table(h).metatable() {
                    Some(mt) => self.heap.tm(mt, Tm::NewIndex),
                    None => Value::Nil,
                };
                if tm.is_nil() || !self.heap.raw_get(h, key).is_nil() {
                    if let Err(e) = self.heap.raw_set(h, key, val) {
                        return Err(self.set_error(e));
                    }
                    return Ok(Set::Done);
                }
                tm
            } else {
                let tm = self.heap.tm_of(t, Tm::NewIndex);
                if tm.is_nil() {
                    return Err(self.type_error(t, origin, "index"));
                }
                tm
            };
            if tm.is_function() {
                return Ok(Set::Call(tm, t));
            }
            t = tm;
            origin = Operand::None;
        }
        Err(self.rt_error("'__newindex' chain too long; possible loop"))
    }

    /// The binary metamethod for `event` from `a`, else `b` (`callbinTM`'s lookup).
    pub(crate) fn bin_tm(&self, a: Value, b: Value, event: Tm) -> Value {
        let tm = self.heap.tm_of(a, event);
        if tm.is_nil() {
            self.heap.tm_of(b, event)
        } else {
            tm
        }
    }

    /// The error when no metamethod implements arithmetic `event` (`luaT_trybinTM`).
    pub(crate) fn arith_error(
        &mut self,
        a: Value,
        b: Value,
        oa: Operand,
        ob: Operand,
        event: Tm,
    ) -> crate::Error {
        let bitwise = matches!(
            event,
            Tm::BAnd | Tm::BOr | Tm::BXor | Tm::Shl | Tm::Shr | Tm::BNot
        );
        if bitwise && a.is_number() && b.is_number() {
            // luaG_tointerror
            let (v, o) = if number::to_integer_ns(a, F2I::Exact).is_none() {
                (a, oa)
            } else {
                (b, ob)
            };
            let _ = v;
            let info = self.var_info(o);
            return self.rt_error(format!("number{info} has no integer representation"));
        }
        let msg = if bitwise {
            "perform bitwise operation on"
        } else {
            "perform arithmetic on"
        };
        // luaG_opinterror: blame the first operand that is not a number
        let (v, o) = if !a.is_number() { (a, oa) } else { (b, ob) };
        self.type_error(v, o, msg)
    }

    /// The error for comparing values with no `__lt`/`__le` (`luaG_ordererror`).
    pub(crate) fn order_error(&mut self, a: Value, b: Value) -> crate::Error {
        let t1 = self.type_name_of(a);
        let t2 = self.type_name_of(b);
        if t1 == t2 {
            self.rt_error(format!("attempt to compare two {t1} values"))
        } else {
            self.rt_error(format!("attempt to compare {t1} with {t2}"))
        }
    }

    /// Converts a number in a stack slot to a string for concatenation (`luaO_tostring`),
    /// returning whether the slot now holds a string.
    pub(crate) fn tostring_slot(&mut self, slot: usize) -> bool {
        match self.st.stack[slot] {
            Value::Str(_) => true,
            v @ (Value::Int(_) | Value::Float(_)) => {
                let s = fmt_number(v);
                self.st.stack[slot] = self.heap.str_value(s);
                true
            }
            _ => false,
        }
    }

    /// `a < b` without metamethods, if it is defined that way (numbers, strings).
    pub(crate) fn raw_lt(&self, a: Value, b: Value) -> Option<bool> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Some(x < y),
            _ if a.is_number() && b.is_number() => Some(number::num_lt(a, b)),
            (Value::Str(x), Value::Str(y)) => {
                Some(self.heap.string(x).as_bytes() < self.heap.string(y).as_bytes())
            }
            _ => None,
        }
    }

    /// `a <= b` without metamethods, if it is defined that way.
    pub(crate) fn raw_le(&self, a: Value, b: Value) -> Option<bool> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Some(x <= y),
            _ if a.is_number() && b.is_number() => Some(number::num_le(a, b)),
            (Value::Str(x), Value::Str(y)) => {
                Some(self.heap.string(x).as_bytes() <= self.heap.string(y).as_bytes())
            }
            _ => None,
        }
    }

    /// Equality: `Ok(result)` when decided without a metamethod, else `Err(__eq)`.
    pub(crate) fn eq_or_tm(&self, a: Value, b: Value) -> Result<bool, Value> {
        match (a, b) {
            (Value::Table(x), Value::Table(y)) => {
                if x == y {
                    return Ok(true);
                }
                let tm = self.eq_tm(a, b);
                if tm.is_nil() { Ok(false) } else { Err(tm) }
            }
            (Value::UserData(x), Value::UserData(y)) => {
                if x == y {
                    return Ok(true);
                }
                let tm = self.eq_tm(a, b);
                if tm.is_nil() { Ok(false) } else { Err(tm) }
            }
            _ => Ok(self.heap.raw_eq(a, b)),
        }
    }

    fn eq_tm(&self, a: Value, b: Value) -> Value {
        let tm = self.heap.tm_of(a, Tm::Eq);
        if tm.is_nil() {
            self.heap.tm_of(b, Tm::Eq)
        } else {
            tm
        }
    }

    /// Raw arithmetic on two values, raising the division-by-zero errors.
    pub(crate) fn try_arith(&mut self, op: Arith, a: Value, b: Value) -> LuaResult<Option<Value>> {
        match number::raw_arith(op, a, b) {
            None => Ok(None),
            Some(Ok(v)) => Ok(Some(v)),
            Some(Err(e)) => Err(self.rt_error(e.message())),
        }
    }
}

/// `Option::map`-like helper that reads left to right.
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}
