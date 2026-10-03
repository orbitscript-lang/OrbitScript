//! The Lua value type (ADR-1).

use orbit_gc::Gc;

use crate::{Closure, LuaHeap, LuaStr, Table, number};

/// An index into the VM's registry of native functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativeId(pub u32);

/// A Lua value. 16 bytes with a pinned layout (amended ADR-1): the tag byte
/// at offset 0 and an 8-byte payload at offset 8, with fixed tag numbers,
/// which are also what table array parts store. Code outside this crate uses
/// the API, never the layout.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C, u8)]
pub enum Value {
    #[default]
    Nil = 0,
    Bool(bool) = 1,
    Int(i64) = 2,
    Float(f64) = 3,
    Str(Gc<LuaStr>) = 4,
    Table(Gc<Table>) = 5,
    Closure(Gc<Closure>) = 6,
    Native(NativeId) = 7,
    // 8 = UserData and 9 = Thread are reserved.
}

const _: () = assert!(core::mem::size_of::<Value>() == 16);

/// Tag numbers, as stored in table array parts.
pub mod tag {
    pub const NIL: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const INT: u8 = 2;
    pub const FLOAT: u8 = 3;
    pub const STR: u8 = 4;
    pub const TABLE: u8 = 5;
    pub const CLOSURE: u8 = 6;
    pub const NATIVE: u8 = 7;
}

impl Value {
    /// Lua's `type` name.
    pub fn type_name(self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Closure(_) | Value::Native(_) => "function",
        }
    }

    #[inline(always)]
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }

    /// `nil` and `false` are falsy; everything else is truthy.
    #[inline(always)]
    pub fn is_falsy(self) -> bool {
        matches!(self, Value::Nil | Value::Bool(false))
    }

    #[inline(always)]
    pub fn is_function(self) -> bool {
        matches!(self, Value::Closure(_) | Value::Native(_))
    }

    /// The tag byte.
    #[inline(always)]
    pub fn tag(self) -> u8 {
        match self {
            Value::Nil => tag::NIL,
            Value::Bool(_) => tag::BOOL,
            Value::Int(_) => tag::INT,
            Value::Float(_) => tag::FLOAT,
            Value::Str(_) => tag::STR,
            Value::Table(_) => tag::TABLE,
            Value::Closure(_) => tag::CLOSURE,
            Value::Native(_) => tag::NATIVE,
        }
    }

    /// Split into tag and payload, the form table array parts store.
    #[inline(always)]
    pub fn to_bits(self) -> (u8, u64) {
        match self {
            Value::Nil => (tag::NIL, 0),
            Value::Bool(b) => (tag::BOOL, b as u64),
            Value::Int(i) => (tag::INT, i as u64),
            Value::Float(f) => (tag::FLOAT, f.to_bits()),
            Value::Str(s) => (tag::STR, s.to_bits()),
            Value::Table(t) => (tag::TABLE, t.to_bits()),
            Value::Closure(c) => (tag::CLOSURE, c.to_bits()),
            Value::Native(n) => (tag::NATIVE, n.0 as u64),
        }
    }

    /// Inverse of [`Value::to_bits`]. An unknown tag gives `nil`.
    #[inline(always)]
    pub fn from_bits(tag: u8, payload: u64) -> Value {
        match tag {
            tag::BOOL => Value::Bool(payload != 0),
            tag::INT => Value::Int(payload as i64),
            tag::FLOAT => Value::Float(f64::from_bits(payload)),
            tag::STR => Value::Str(Gc::from_bits(payload)),
            tag::TABLE => Value::Table(Gc::from_bits(payload)),
            tag::CLOSURE => Value::Closure(Gc::from_bits(payload)),
            tag::NATIVE => Value::Native(NativeId(payload as u32)),
            _ => Value::Nil,
        }
    }

    /// The number, if this is one (no string coercion).
    #[inline(always)]
    pub fn as_f64(self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(i as f64),
            Value::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Identity for `table: 0x...` style printing, stable for the object's
    /// lifetime and distinct between live objects of different types.
    pub fn identity(self) -> Option<u64> {
        let (tag, payload) = match self {
            Value::Table(t) => (tag::TABLE, t.index() as u64),
            Value::Closure(c) => (tag::CLOSURE, c.index() as u64),
            Value::Native(n) => (tag::NATIVE, n.0 as u64),
            Value::Str(s) => (tag::STR, s.index() as u64),
            _ => return None,
        };
        // Looks like an address: a fixed base, the type in the high bits and
        // the slot index scaled by a plausible object size.
        Some(0x5500_0000_0000 | (tag as u64) << 36 | payload << 5)
    }
}

/// Raw equality (`rawequal`, and `==` without metamethods): numbers compare
/// mathematically across subtypes, strings by content, everything else by
/// identity.
pub fn raw_eq(heap: &LuaHeap, a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i)) => {
            number::int_eq_float(i, f)
        }
        (Value::Str(x), Value::Str(y)) => x == y || str_eq(heap, x, y),
        (Value::Table(x), Value::Table(y)) => x == y,
        (Value::Closure(x), Value::Closure(y)) => x == y,
        (Value::Native(x), Value::Native(y)) => x == y,
        _ => false,
    }
}

/// Content equality of two distinct string handles. Short strings are
/// interned, so two different short handles are never equal.
#[inline]
pub fn str_eq(heap: &LuaHeap, a: Gc<LuaStr>, b: Gc<LuaStr>) -> bool {
    let (x, y) = (heap.get(a), heap.get(b));
    !(x.is_short() && y.is_short()) && x.as_bytes() == y.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_round_trip() {
        for v in [
            Value::Nil,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(-5),
            Value::Float(-0.0),
            Value::Native(NativeId(7)),
            Value::Table(Gc::from_bits(3)),
        ] {
            let (t, p) = v.to_bits();
            assert_eq!(t, v.tag());
            let w = Value::from_bits(t, p);
            assert_eq!(w.to_bits(), (t, p));
        }
    }

    #[test]
    fn truthiness() {
        assert!(Value::Nil.is_falsy());
        assert!(Value::Bool(false).is_falsy());
        assert!(!Value::Int(0).is_falsy());
        assert!(!Value::Bool(true).is_falsy());
    }
}
