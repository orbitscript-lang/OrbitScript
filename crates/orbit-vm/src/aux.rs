//! Helpers for native functions: argument checks with the reference's messages,
//! `tostring` conversion and function names (`lauxlib.c`).

use crate::error::{Error, LuaResult};
use crate::number::{self, F2I};
use crate::state::Vm;
use orbit_core::Value;
use orbit_core::value::{StrRef, TableRef};

/// Registry key of the table of loaded modules (`LUA_LOADED_TABLE`).
pub const LOADED_TABLE: &[u8] = b"_LOADED";

/// A stable number identifying a heap object, for `%p`-style output such as
/// `table: 0x...`. Values that are not objects give 0.
pub fn value_addr(heap: &orbit_core::Heap, v: Value) -> u64 {
    let (kind, index) = match v {
        Value::Str(s) => (1u64, s.index()),
        Value::Table(t) => (2, t.index()),
        Value::LuaFn(f) => (3, f.index()),
        Value::NativeFn(f) => (4, f.index()),
        Value::LightNative(id) => (5, id.0),
        Value::UserData(u) => (6, u.index()),
        Value::Thread(t) => (7, t.index()),
        _ => return 0,
    };
    let _ = heap;
    // Look like a user-space address: never zero, kind in the high bits.
    0x5500_0000_0000 | (kind << 36) | (u64::from(index) << 4)
}

impl Vm {
    /// "bad argument #arg to 'name' (extramsg)" (`luaL_argerror`).
    pub fn arg_error(&mut self, arg: usize, extramsg: &str) -> Error {
        let Some(fi) = self.st.frames.len().checked_sub(1) else {
            return self.error(format!("bad argument #{arg} ({extramsg})"));
        };
        let mut arg = arg;
        let named = self.func_name_for_frame(fi);
        if let Some(("method", name)) = named.as_ref().map(|(k, n)| (*k, n.as_str())) {
            arg -= 1;
            if arg == 0 {
                let name = name.to_string();
                return self.error(format!("calling '{name}' on bad self ({extramsg})"));
            }
        }
        let name = match named {
            Some((_, n)) => n,
            None => {
                let f = self.st.stack[self.st.frames[fi].func];
                self.global_func_name(f).unwrap_or_else(|| "?".to_string())
            }
        };
        self.error(format!("bad argument #{arg} to '{name}' ({extramsg})"))
    }

    /// "bad argument ... (tname expected, got type)" (`luaL_typeerror`).
    pub fn type_arg_error(&mut self, arg: usize, tname: &str) -> Error {
        let got = if arg > self.top() {
            "no value".to_string()
        } else {
            self.type_name_of(self.arg(arg))
        };
        self.arg_error(arg, &format!("{tname} expected, got {got}"))
    }

    /// The name of `f` in the loaded modules, like `print` or `string.format`
    /// (`pushglobalfuncname`).
    pub fn global_func_name(&self, f: Value) -> Option<String> {
        let Value::Table(loaded) = self.heap.raw_get_name(self.registry, LOADED_TABLE) else {
            return None;
        };
        let mut key = Value::Nil;
        while let Ok(Some((k, v))) = self.heap.next(loaded, key) {
            key = k;
            let (Value::Str(modname), Value::Table(m)) = (k, v) else {
                continue;
            };
            let mut k2 = Value::Nil;
            while let Ok(Some((fk, fv))) = self.heap.next(m, k2) {
                k2 = fk;
                if let Value::Str(fname) = fk
                    && self.heap.raw_eq(fv, f)
                {
                    let m = String::from_utf8_lossy(self.heap.string(modname).as_bytes());
                    let n = String::from_utf8_lossy(self.heap.string(fname).as_bytes());
                    return Some(if m == "_G" {
                        n.into_owned()
                    } else {
                        format!("{m}.{n}")
                    });
                }
            }
        }
        None
    }

    /// Argument `arg` must be present (`luaL_checkany`).
    pub fn check_any(&mut self, arg: usize) -> LuaResult<Value> {
        if arg > self.top() {
            return Err(self.arg_error(arg, "value expected"));
        }
        Ok(self.arg(arg))
    }

    /// Argument `arg` must be a table.
    pub fn check_table(&mut self, arg: usize) -> LuaResult<TableRef> {
        match self.arg(arg) {
            Value::Table(t) => Ok(t),
            _ => Err(self.type_arg_error(arg, "table")),
        }
    }

    /// Argument `arg` must be a function.
    pub fn check_function(&mut self, arg: usize) -> LuaResult<Value> {
        let v = self.arg(arg);
        if v.is_function() {
            Ok(v)
        } else {
            Err(self.type_arg_error(arg, "function"))
        }
    }

    /// Argument `arg` as an integer (`luaL_checkinteger`): integers, floats with an
    /// exact integer value, and strings convertible to one.
    pub fn check_integer(&mut self, arg: usize) -> LuaResult<i64> {
        let v = self.arg(arg);
        match number::to_integer(&self.heap, v, F2I::Exact) {
            Some(n) => Ok(n),
            None => {
                if number::to_number(&self.heap, v).is_some() {
                    Err(self.arg_error(arg, "number has no integer representation"))
                } else {
                    Err(self.type_arg_error(arg, "number"))
                }
            }
        }
    }

    /// Like [`Vm::check_integer`], with a default for `nil` or a missing argument.
    pub fn opt_integer(&mut self, arg: usize, default: i64) -> LuaResult<i64> {
        if self.arg(arg).is_nil() {
            Ok(default)
        } else {
            self.check_integer(arg)
        }
    }

    /// Argument `arg` as a number (`luaL_checknumber`), converting strings.
    pub fn check_number(&mut self, arg: usize) -> LuaResult<Value> {
        let v = self.arg(arg);
        match number::to_number(&self.heap, v) {
            Some(n) => Ok(n),
            None => Err(self.type_arg_error(arg, "number")),
        }
    }

    /// Argument `arg` as a string (`luaL_checklstring`). Numbers are converted, and the
    /// argument slot is replaced with the string, as in the reference.
    pub fn check_string(&mut self, arg: usize) -> LuaResult<StrRef> {
        match self.arg(arg) {
            Value::Str(s) => Ok(s),
            v @ (Value::Int(_) | Value::Float(_)) => {
                let s = self.heap.intern(number::fmt_number(v).as_bytes());
                self.set(arg, Value::Str(s));
                Ok(s)
            }
            _ => Err(self.type_arg_error(arg, "string")),
        }
    }

    /// Like [`Vm::check_string`], returning `None` for `nil` or a missing argument.
    pub fn opt_string(&mut self, arg: usize) -> LuaResult<Option<StrRef>> {
        if self.arg(arg).is_nil() {
            Ok(None)
        } else {
            self.check_string(arg).map(Some)
        }
    }

    /// Field `name` of `v`'s metatable, or `nil` (`luaL_getmetafield`).
    pub fn meta_field(&self, v: Value, name: &[u8]) -> Value {
        match self.heap.metatable(v) {
            Some(mt) => self.heap.raw_get_name(mt, name),
            None => Value::Nil,
        }
    }

    /// Converts any value to a string the way `tostring` does (`luaL_tolstring`),
    /// calling `__tostring` if present.
    pub fn to_display(&mut self, v: Value) -> LuaResult<StrRef> {
        let tm = self.meta_field(v, b"__tostring");
        if !tm.is_nil() {
            self.check_stack(3)?;
            self.push(tm);
            self.push(v);
            self.call(1, Some(1))?;
            return match self.pop() {
                Value::Str(s) => Ok(s),
                r @ (Value::Int(_) | Value::Float(_)) => {
                    Ok(self.heap.intern(number::fmt_number(r).as_bytes()))
                }
                _ => Err(self.error("'__tostring' must return a string")),
            };
        }
        let s = match v {
            Value::Str(s) => return Ok(s),
            Value::Int(_) | Value::Float(_) => number::fmt_number(v),
            Value::Bool(b) => b.to_string(),
            Value::Nil => "nil".to_string(),
            _ => {
                let kind = match self.meta_field(v, b"__name") {
                    Value::Str(s) => {
                        String::from_utf8_lossy(self.heap.string(s).as_bytes()).into_owned()
                    }
                    _ => v.type_name().to_string(),
                };
                format!("{kind}: {:#x}", value_addr(&self.heap, v))
            }
        };
        Ok(self.heap.intern(s.as_bytes()))
    }

    /// The table of loaded modules (`registry._LOADED`), created on first use.
    pub fn loaded_table(&mut self) -> TableRef {
        match self.heap.raw_get_name(self.registry, LOADED_TABLE) {
            Value::Table(t) => t,
            _ => {
                let t = self.heap.new_table();
                self.heap
                    .raw_set_name(self.registry, LOADED_TABLE, Value::Table(t))
                    .expect("string key");
                t
            }
        }
    }

    /// Creates a library table from `funcs`, registers it as module `name` and, unless
    /// `name` is `_G`, stores it in the global of that name. Returns the table.
    pub fn new_lib(&mut self, name: &str, funcs: &[(&'static str, crate::NativeFn)]) -> TableRef {
        let t = if name == "_G" {
            self.globals
        } else {
            self.heap.new_table()
        };
        for &(fname, f) in funcs {
            let v = self.register(fname, f);
            self.heap
                .raw_set_name(t, fname.as_bytes(), v)
                .expect("string key");
        }
        let loaded = self.loaded_table();
        self.heap
            .raw_set_name(loaded, name.as_bytes(), Value::Table(t))
            .expect("string key");
        if name != "_G" {
            self.heap
                .raw_set_name(self.globals, name.as_bytes(), Value::Table(t))
                .expect("string key");
        }
        t
    }
}
