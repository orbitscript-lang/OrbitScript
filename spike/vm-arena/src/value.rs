//! Values and handles. `Value` is 16 bytes; handles are `u32` indexes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StrRef(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TabRef(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FnRef(pub u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UvRef(pub u32);

#[derive(Clone, Copy, Debug, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(StrRef),
    Table(TabRef),
    Func(FnRef),
}

impl Value {
    #[inline(always)]
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
    #[inline(always)]
    pub fn truthy(self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
}

/// A table key. Strings are canonical (interned) handles, so every variant
/// hashes and compares without touching the heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Float(u64),
    Bool(bool),
    Str(u32),
    Table(u32),
    Func(u32),
}

impl Key {
    /// Non-string conversion; `None` for nil and NaN.
    #[inline(always)]
    pub fn from_value(v: Value) -> Option<Key> {
        Some(match v {
            Value::Nil => return None,
            Value::Bool(b) => Key::Bool(b),
            Value::Int(i) => Key::Int(i),
            Value::Float(f) => {
                if f.is_nan() {
                    return None;
                }
                if f == (f as i64) as f64 {
                    Key::Int(f as i64)
                } else {
                    Key::Float(f.to_bits())
                }
            }
            Value::Str(s) => Key::Str(s.0),
            Value::Table(t) => Key::Table(t.0),
            Value::Func(f) => Key::Func(f.0),
        })
    }
}
