//! Re-entrant operations for native functions, with metamethods: indexing, length,
//! comparison and calls (`lua_geti`, `lua_seti`, `lua_len`, `lua_compare`, ...).
//! `lua_gettable` is [`Vm::index_value`] in `baselib.rs`.
//!
//! Metamethods run through [`Vm::call`], so they cannot yield, as with the reference's
//! C API functions without a continuation.

use crate::debug::Operand;
use crate::error::LuaResult;
use crate::meta::Set;
use crate::state::Vm;
use orbit_core::{Tm, Value};

impl Vm {
    /// Calls `f` with `args`, returning its first result (or `nil`).
    pub fn call_value(&mut self, f: Value, args: &[Value]) -> LuaResult<Value> {
        self.check_stack(args.len() + 1)?;
        self.push(f);
        for &a in args {
            self.push(a);
        }
        self.call(args.len(), Some(1))?;
        Ok(self.pop())
    }

    /// `t[i]` with `__index` (`lua_geti`).
    pub fn get_index(&mut self, t: Value, i: i64) -> LuaResult<Value> {
        if let Value::Table(h) = t {
            let v = self.heap.raw_get_int(h, i);
            if !v.is_nil() {
                return Ok(v);
            }
        }
        self.index_value(t, Value::Int(i))
    }

    /// `t[k] = v` with `__newindex` (`lua_settable`).
    pub fn set_table(&mut self, t: Value, k: Value, v: Value) -> LuaResult<()> {
        match self.finish_set(t, k, v, Operand::None)? {
            Set::Done => Ok(()),
            Set::Call(tm, t) => {
                self.check_stack(4)?;
                self.push(tm);
                self.push(t);
                self.push(k);
                self.push(v);
                self.call(3, Some(0))
            }
        }
    }

    /// `t[i] = v` with `__newindex` (`lua_seti`).
    pub fn set_index(&mut self, t: Value, i: i64, v: Value) -> LuaResult<()> {
        self.set_table(t, Value::Int(i), v)
    }

    /// `#v` with `__len` (`lua_len`).
    pub fn length(&mut self, v: Value) -> LuaResult<Value> {
        let tm = match v {
            Value::Str(s) => return Ok(Value::Int(self.heap.string(s).len() as i64)),
            Value::Table(h) => {
                let tm = self.heap.tm_of(v, Tm::Len);
                if tm.is_nil() {
                    return Ok(Value::Int(self.heap.raw_len(h) as i64));
                }
                tm
            }
            _ => {
                let tm = self.heap.tm_of(v, Tm::Len);
                if tm.is_nil() {
                    let t = self.type_name_of(v);
                    return Err(self.rt_error(format!("attempt to get length of a {t} value")));
                }
                tm
            }
        };
        self.call_value(tm, &[v, v])
    }

    /// `#v` as an integer (`luaL_len`).
    pub fn length_int(&mut self, v: Value) -> LuaResult<i64> {
        match self.length(v)? {
            Value::Int(n) => Ok(n),
            Value::Float(f)
                if crate::number::float_to_integer(f, crate::number::F2I::Exact).is_some() =>
            {
                Ok(f as i64)
            }
            _ => Err(self.error("object length is not an integer")),
        }
    }

    /// `a < b` with `__lt` (`lua_compare(L, a, b, LUA_OPLT)`).
    pub fn less_than(&mut self, a: Value, b: Value) -> LuaResult<bool> {
        if let Some(r) = self.raw_lt(a, b) {
            return Ok(r);
        }
        let tm = self.bin_tm(a, b, Tm::Lt);
        if tm.is_nil() {
            return Err(self.order_error(a, b));
        }
        Ok(self.call_value(tm, &[a, b])?.is_truthy())
    }

    /// `a <= b` with `__le` (`lua_compare(L, a, b, LUA_OPLE)`).
    pub fn less_equal(&mut self, a: Value, b: Value) -> LuaResult<bool> {
        if let Some(r) = self.raw_le(a, b) {
            return Ok(r);
        }
        let tm = self.bin_tm(a, b, Tm::Le);
        if tm.is_nil() {
            return Err(self.order_error(a, b));
        }
        Ok(self.call_value(tm, &[a, b])?.is_truthy())
    }

    /// `a == b` with `__eq` (`lua_compare(L, a, b, LUA_OPEQ)`).
    pub fn equals(&mut self, a: Value, b: Value) -> LuaResult<bool> {
        match self.eq_or_tm(a, b) {
            Ok(r) => Ok(r),
            Err(tm) => Ok(self.call_value(tm, &[a, b])?.is_truthy()),
        }
    }
}
