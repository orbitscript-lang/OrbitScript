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

/// A [`Value`]'s type tag, as packed storage (table array parts and hash
/// keys) keeps it: one byte with the pinned tag numbers (amended ADR-1).
/// Being an enum, a stored tag is always a valid one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Tag {
    #[default]
    Nil = 0,
    Bool = 1,
    Int = 2,
    Float = 3,
    Str = 4,
    Table = 5,
    Closure = 6,
    Native = 7,
}

impl Tag {
    /// The tag with byte value `b`, if there is one.
    #[inline(always)]
    pub fn from_u8(b: u8) -> Option<Tag> {
        Some(match b {
            0 => Tag::Nil,
            1 => Tag::Bool,
            2 => Tag::Int,
            3 => Tag::Float,
            4 => Tag::Str,
            5 => Tag::Table,
            6 => Tag::Closure,
            7 => Tag::Native,
            _ => return None,
        })
    }
}

/// A [`Value`]'s 8-byte payload, separated from its [`Tag`] for packed
/// storage. Opaque: only [`Value::pack`] makes one and only
/// [`Value::unpack`] reads one, so a payload can't be mistaken for a number
/// or a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Payload(u64);

impl Payload {
    /// The raw bits, for hashing.
    #[inline(always)]
    pub(crate) fn bits(self) -> u64 {
        self.0
    }
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

    /// The type tag.
    #[inline(always)]
    pub fn tag(self) -> Tag {
        match self {
            Value::Nil => Tag::Nil,
            Value::Bool(_) => Tag::Bool,
            Value::Int(_) => Tag::Int,
            Value::Float(_) => Tag::Float,
            Value::Str(_) => Tag::Str,
            Value::Table(_) => Tag::Table,
            Value::Closure(_) => Tag::Closure,
            Value::Native(_) => Tag::Native,
        }
    }

    /// Split into tag and payload, the form packed storage keeps.
    #[inline(always)]
    pub fn pack(self) -> (Tag, Payload) {
        let (tag, bits) = match self {
            Value::Nil => (Tag::Nil, 0),
            Value::Bool(b) => (Tag::Bool, b as u64),
            Value::Int(i) => (Tag::Int, i as u64),
            Value::Float(f) => (Tag::Float, f.to_bits()),
            Value::Str(s) => (Tag::Str, s.to_bits()),
            Value::Table(t) => (Tag::Table, t.to_bits()),
            Value::Closure(c) => (Tag::Closure, c.to_bits()),
            Value::Native(n) => (Tag::Native, n.0 as u64),
        };
        (tag, Payload(bits))
    }

    /// Inverse of [`Value::pack`].
    #[inline(always)]
    pub fn unpack(tag: Tag, payload: Payload) -> Value {
        let p = payload.0;
        match tag {
            Tag::Nil => Value::Nil,
            Tag::Bool => Value::Bool(p != 0),
            Tag::Int => Value::Int(p as i64),
            Tag::Float => Value::Float(f64::from_bits(p)),
            Tag::Str => Value::Str(Gc::from_bits(p)),
            Tag::Table => Value::Table(Gc::from_bits(p)),
            Tag::Closure => Value::Closure(Gc::from_bits(p)),
            Tag::Native => Value::Native(NativeId(p as u32)),
        }
    }

    /// Whether `self` and `other` are the same value bit for bit (identity
    /// for objects, no numeric conversion).
    #[inline]
    pub fn same(self, other: Value) -> bool {
        self.pack() == other.pack()
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
            Value::Table(t) => (Tag::Table, t.index() as u64),
            Value::Closure(c) => (Tag::Closure, c.index() as u64),
            Value::Native(n) => (Tag::Native, n.0 as u64),
            Value::Str(s) => (Tag::Str, s.index() as u64),
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
    fn tags_match_the_pinned_numbers() {
        // amended ADR-1: tag bytes are part of the pinned layout.
        let pinned = [
            (Tag::Nil, 0),
            (Tag::Bool, 1),
            (Tag::Int, 2),
            (Tag::Float, 3),
            (Tag::Str, 4),
            (Tag::Table, 5),
            (Tag::Closure, 6),
            (Tag::Native, 7),
        ];
        for (t, b) in pinned {
            assert_eq!(t as u8, b);
            assert_eq!(Tag::from_u8(b), Some(t));
        }
        assert_eq!(Tag::from_u8(8), None);
        assert_eq!(std::mem::size_of::<Tag>(), 1);
    }

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
            let (t, p) = v.pack();
            assert_eq!(t, v.tag());
            let w = Value::unpack(t, p);
            assert!(w.same(v));
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
