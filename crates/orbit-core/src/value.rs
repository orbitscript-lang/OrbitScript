//! `Value`, the 16-byte `Copy` representation of every Lua value (ADR-1).

use crate::object::{LuaClosure, NativeClosure, Thread, UserData};
use crate::string::LuaStr;
use crate::table::Table;
use orbit_gc::Gc;
use std::fmt;

/// Handle to an interned or long string.
pub type StrRef = Gc<LuaStr>;
/// Handle to a table.
pub type TableRef = Gc<Table>;
/// Handle to a Lua function (a closure over a prototype).
pub type LuaFnRef = Gc<LuaClosure>;
/// Handle to a native function that carries upvalues.
pub type NativeFnRef = Gc<NativeClosure>;
/// Handle to a full userdata.
pub type UserDataRef = Gc<UserData>;
/// Handle to a coroutine.
pub type ThreadRef = Gc<Thread>;

/// A native function with no upvalues. It is an index into the VM's registry of native
/// function pointers, so it needs no allocation (Lua's "light C function").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeId(pub u32);

/// A Lua value.
///
/// Code outside `orbit-core` uses this API rather than relying on the layout, so the layout
/// can change after measurement (ADR-1).
#[derive(Clone, Copy, Default)]
pub enum Value {
    /// `nil`.
    #[default]
    Nil,
    /// `true` or `false`.
    Bool(bool),
    /// An integer-subtype number.
    Int(i64),
    /// A float-subtype number.
    Float(f64),
    /// A string.
    Str(StrRef),
    /// A table.
    Table(TableRef),
    /// A Lua function.
    LuaFn(LuaFnRef),
    /// A native function with upvalues.
    NativeFn(NativeFnRef),
    /// A native function without upvalues.
    LightNative(NativeId),
    /// A full userdata.
    UserData(UserDataRef),
    /// A coroutine.
    Thread(ThreadRef),
}

const _: () = assert!(size_of::<Value>() == 16);

/// The basic Lua types, as `type` reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// `nil`
    Nil,
    /// `boolean`
    Boolean,
    /// `number`
    Number,
    /// `string`
    String,
    /// `table`
    Table,
    /// `function`
    Function,
    /// `userdata`
    UserData,
    /// `thread`
    Thread,
}

impl Type {
    /// The name `type()` returns.
    pub const fn name(self) -> &'static str {
        match self {
            Type::Nil => "nil",
            Type::Boolean => "boolean",
            Type::Number => "number",
            Type::String => "string",
            Type::Table => "table",
            Type::Function => "function",
            Type::UserData => "userdata",
            Type::Thread => "thread",
        }
    }

    /// Number of basic types; `Type as usize` is below this.
    pub const COUNT: usize = 8;
}

/// Why a value can't be used as a table key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The key is `nil`.
    Nil,
    /// The key is a float NaN.
    NaN,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            KeyError::Nil => "table index is nil",
            KeyError::NaN => "table index is NaN",
        })
    }
}

impl std::error::Error for KeyError {}

impl Value {
    /// The basic type of the value.
    #[inline]
    pub const fn type_of(self) -> Type {
        match self {
            Value::Nil => Type::Nil,
            Value::Bool(_) => Type::Boolean,
            Value::Int(_) | Value::Float(_) => Type::Number,
            Value::Str(_) => Type::String,
            Value::Table(_) => Type::Table,
            Value::LuaFn(_) | Value::NativeFn(_) | Value::LightNative(_) => Type::Function,
            Value::UserData(_) => Type::UserData,
            Value::Thread(_) => Type::Thread,
        }
    }

    /// The name `type()` returns for this value.
    pub const fn type_name(self) -> &'static str {
        self.type_of().name()
    }

    /// Whether the value is `nil`.
    #[inline(always)]
    pub const fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Lua truthiness: everything except `nil` and `false` is true.
    #[inline(always)]
    pub const fn is_truthy(self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Whether the value is `nil` or `false`.
    #[inline(always)]
    pub const fn is_falsy(self) -> bool {
        !self.is_truthy()
    }

    /// Whether the value is a number of either subtype.
    #[inline]
    pub const fn is_number(self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_))
    }

    /// Whether the value is a function of any kind.
    #[inline]
    pub const fn is_function(self) -> bool {
        matches!(
            self,
            Value::LuaFn(_) | Value::NativeFn(_) | Value::LightNative(_)
        )
    }

    /// Whether the value refers to a garbage-collected object.
    #[inline]
    pub const fn is_collectable(self) -> bool {
        matches!(
            self,
            Value::Str(_)
                | Value::Table(_)
                | Value::LuaFn(_)
                | Value::NativeFn(_)
                | Value::UserData(_)
                | Value::Thread(_)
        )
    }

    /// The number as a float, converting integers.
    #[inline]
    pub fn as_float(self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            _ => None,
        }
    }

    /// The number as an integer, if it has an exact integer representation. Strings are
    /// not converted.
    #[inline]
    pub fn as_int(self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(i),
            Value::Float(f) => float_to_int(f),
            _ => None,
        }
    }

    /// The string handle, if the value is a string.
    #[inline]
    pub const fn as_str(self) -> Option<StrRef> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The table handle, if the value is a table.
    #[inline]
    pub const fn as_table(self) -> Option<TableRef> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Raw equality (`rawequal`): numbers compare by mathematical value across subtypes,
    /// everything else by identity. Strings compare by identity too, so both must be
    /// canonical (interned short strings, or long strings canonicalized for use as keys)
    /// for this to match Lua; otherwise compare their bytes.
    #[inline]
    pub fn raw_eq(self, other: Value) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i)) => {
                float_to_int(f) == Some(i)
            }
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => a == b,
            (Value::LuaFn(a), Value::LuaFn(b)) => a == b,
            (Value::NativeFn(a), Value::NativeFn(b)) => a == b,
            (Value::LightNative(a), Value::LightNative(b)) => a == b,
            (Value::UserData(a), Value::UserData(b)) => a == b,
            (Value::Thread(a), Value::Thread(b)) => a == b,
            _ => false,
        }
    }

    /// Normalizes a value for use as a table key: floats with an integer value become
    /// integers, and `nil` and NaN are rejected. String canonicalization is the heap's job.
    #[inline]
    pub fn normalize_key(self) -> Result<Value, KeyError> {
        match self {
            Value::Nil => Err(KeyError::Nil),
            Value::Float(f) => match float_to_int(f) {
                Some(i) => Ok(Value::Int(i)),
                None if f.is_nan() => Err(KeyError::NaN),
                None => Ok(self),
            },
            v => Ok(v),
        }
    }

    /// Identity used by table keys: two normalized keys are the same entry iff their
    /// identities are equal. Floats compare by bits here, which matches `==` because keys
    /// are normalized (no NaN, and ±0.0 become the integer 0).
    #[inline(always)]
    pub(crate) fn key_identity(self) -> (u8, u64) {
        self.pack()
    }

    /// Packs the value into a tag byte and an 8-byte payload, for the split-tag array part
    /// and the hash nodes.
    #[inline(always)]
    pub(crate) fn pack(self) -> (u8, u64) {
        match self {
            Value::Nil => (tag::NIL, 0),
            Value::Bool(false) => (tag::FALSE, 0),
            Value::Bool(true) => (tag::TRUE, 0),
            Value::Int(i) => (tag::INT, i as u64),
            Value::Float(f) => (tag::FLOAT, f.to_bits()),
            Value::Str(s) => (tag::STR, u64::from(s.index())),
            Value::Table(t) => (tag::TABLE, u64::from(t.index())),
            Value::LuaFn(f) => (tag::LUA_FN, u64::from(f.index())),
            Value::NativeFn(f) => (tag::NATIVE_FN, u64::from(f.index())),
            Value::LightNative(n) => (tag::LIGHT_NATIVE, u64::from(n.0)),
            Value::UserData(u) => (tag::USERDATA, u64::from(u.index())),
            Value::Thread(t) => (tag::THREAD, u64::from(t.index())),
        }
    }

    /// Inverse of [`Value::pack`]. Unknown tags (such as dead hash keys) decode as `nil`.
    #[inline(always)]
    pub(crate) fn unpack(t: u8, payload: u64) -> Value {
        match t {
            tag::FALSE => Value::Bool(false),
            tag::TRUE => Value::Bool(true),
            tag::INT => Value::Int(payload as i64),
            tag::FLOAT => Value::Float(f64::from_bits(payload)),
            tag::STR => Value::Str(Gc::from_index(payload as u32)),
            tag::TABLE => Value::Table(Gc::from_index(payload as u32)),
            tag::LUA_FN => Value::LuaFn(Gc::from_index(payload as u32)),
            tag::NATIVE_FN => Value::NativeFn(Gc::from_index(payload as u32)),
            tag::LIGHT_NATIVE => Value::LightNative(NativeId(payload as u32)),
            tag::USERDATA => Value::UserData(Gc::from_index(payload as u32)),
            tag::THREAD => Value::Thread(Gc::from_index(payload as u32)),
            _ => Value::Nil,
        }
    }
}

/// Tag bytes for packed values.
pub(crate) mod tag {
    pub const NIL: u8 = 0;
    pub const FALSE: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const INT: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const STR: u8 = 5;
    pub const TABLE: u8 = 6;
    pub const LUA_FN: u8 = 7;
    pub const NATIVE_FN: u8 = 8;
    pub const LIGHT_NATIVE: u8 = 9;
    pub const USERDATA: u8 = 10;
    pub const THREAD: u8 = 11;
}

/// Converts a float to an integer if it has an exact integer value in range
/// (`luaV_flttointns` with `F2Ieq`).
#[inline]
pub fn float_to_int(f: f64) -> Option<i64> {
    // -2^63 is exact as a float; 2^63 is the first float out of range.
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        let i = f as i64;
        if i as f64 == f {
            return Some(i);
        }
    }
    None
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Value::Nil => f.write_str("nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x:?}"),
            Value::Str(s) => write!(f, "string#{}", s.index()),
            Value::Table(t) => write!(f, "table#{}", t.index()),
            Value::LuaFn(g) => write!(f, "function#{}", g.index()),
            Value::NativeFn(g) => write!(f, "native#{}", g.index()),
            Value::LightNative(n) => write!(f, "light-native#{}", n.0),
            Value::UserData(u) => write!(f, "userdata#{}", u.index()),
            Value::Thread(t) => write!(f, "thread#{}", t.index()),
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

impl From<StrRef> for Value {
    fn from(s: StrRef) -> Self {
        Value::Str(s)
    }
}

impl From<TableRef> for Value {
    fn from(t: TableRef) -> Self {
        Value::Table(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_int_edges() {
        assert_eq!(float_to_int(3.0), Some(3));
        assert_eq!(float_to_int(-0.0), Some(0));
        assert_eq!(float_to_int(3.5), None);
        assert_eq!(float_to_int(f64::NAN), None);
        assert_eq!(float_to_int(f64::INFINITY), None);
        assert_eq!(float_to_int(-9_223_372_036_854_775_808.0), Some(i64::MIN));
        assert_eq!(float_to_int(9_223_372_036_854_775_808.0), None);
    }

    #[test]
    fn key_normalization() {
        assert_eq!(Value::Nil.normalize_key().unwrap_err(), KeyError::Nil);
        assert_eq!(
            Value::Float(f64::NAN).normalize_key().unwrap_err(),
            KeyError::NaN
        );
        assert!(matches!(
            Value::Float(2.0).normalize_key(),
            Ok(Value::Int(2))
        ));
        assert!(matches!(
            Value::Float(-0.0).normalize_key(),
            Ok(Value::Int(0))
        ));
        assert!(matches!(
            Value::Float(0.5).normalize_key(),
            Ok(Value::Float(f)) if f == 0.5
        ));
    }

    #[test]
    fn raw_equality() {
        assert!(Value::Int(1).raw_eq(Value::Float(1.0)));
        assert!(!Value::Int(1).raw_eq(Value::Float(1.5)));
        assert!(!Value::Float(f64::NAN).raw_eq(Value::Float(f64::NAN)));
        assert!(!Value::Int(0).raw_eq(Value::Bool(false)));
        assert!(Value::Nil.raw_eq(Value::Nil));
        let t = Value::Table(Gc::from_index(3));
        assert!(t.raw_eq(Value::Table(Gc::from_index(3))));
        assert!(!t.raw_eq(Value::Table(Gc::from_index(4))));
    }

    #[test]
    fn pack_round_trip() {
        let vals = [
            Value::Nil,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(-7),
            Value::Float(-0.25),
            Value::Str(Gc::from_index(9)),
            Value::Table(Gc::from_index(u32::MAX)),
            Value::LuaFn(Gc::from_index(1)),
            Value::NativeFn(Gc::from_index(2)),
            Value::LightNative(NativeId(5)),
            Value::UserData(Gc::from_index(6)),
            Value::Thread(Gc::from_index(8)),
        ];
        for v in vals {
            let (t, p) = v.pack();
            let back = Value::unpack(t, p);
            assert_eq!(back.pack(), (t, p), "{v:?}");
            assert_eq!(back.type_of(), v.type_of());
        }
    }

    #[test]
    fn types() {
        assert_eq!(Value::LightNative(NativeId(0)).type_name(), "function");
        assert_eq!(Value::Float(1.0).type_name(), "number");
        assert!(Value::Int(0).is_truthy());
        assert!(Value::Bool(false).is_falsy());
    }
}
