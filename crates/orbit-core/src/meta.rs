//! Metatable field names. Tables remember which of these they are known to
//! lack (see [`Table::meta_field`](crate::Table::meta_field)), so the list
//! lives here, where the table code and the collector (which reads `__gc`
//! and `__mode`) can both name a field the same way.

macro_rules! events {
    ($($variant:ident = $name:literal,)*) => {
        /// A metamethod event (or another metatable field the runtime reads).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum Event {
            $($variant,)*
        }

        impl Event {
            /// Every event, in declaration order.
            pub const ALL: &'static [Event] = &[$(Event::$variant,)*];

            /// The metatable field name, such as `__index`.
            pub fn field(self) -> &'static str {
                match self {
                    $(Event::$variant => $name,)*
                }
            }
        }
    };
}

// The first `CACHED` events are the ones tables remember as absent: the
// fields read on every `setmetatable`, collection, field miss, `#`, `==`,
// call and `tostring`. Keep the frequently probed ones first.
events! {
    Index = "__index",
    NewIndex = "__newindex",
    Gc = "__gc",
    Mode = "__mode",
    Len = "__len",
    Eq = "__eq",
    Close = "__close",
    Metatable = "__metatable",
    Call = "__call",
    Name = "__name",
    ToString = "__tostring",
    Pairs = "__pairs",
    Concat = "__concat",
    Lt = "__lt",
    Le = "__le",
    Unm = "__unm",
    Add = "__add",
    Sub = "__sub",
    Mul = "__mul",
    Mod = "__mod",
    Pow = "__pow",
    Div = "__div",
    IDiv = "__idiv",
    BAnd = "__band",
    BOr = "__bor",
    BXor = "__bxor",
    Shl = "__shl",
    Shr = "__shr",
    BNot = "__bnot",
}

/// How many events (the first ones) have a bit in a table's absent-field
/// set.
const CACHED: usize = 16;

const _: () = assert!(CACHED <= u16::BITS as usize && CACHED <= Event::ALL.len());

impl Event {
    /// The name without the underscores, as messages and tracebacks print
    /// it (`metamethod 'index'`).
    pub fn short_name(self) -> &'static str {
        &self.field()[2..]
    }

    /// This event's bit in a table's absent-field set, or 0 for an event
    /// tables don't remember (testing it then always says "not known
    /// absent", and setting it does nothing).
    #[inline(always)]
    pub(crate) fn bit(self) -> u16 {
        if (self as usize) < CACHED {
            1 << self as u32
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_distinct() {
        let mut seen = 0u16;
        for (i, &e) in Event::ALL.iter().enumerate() {
            assert_eq!(e as usize, i);
            assert_eq!(seen & e.bit(), 0);
            assert_eq!(e.bit() != 0, i < CACHED);
            seen |= e.bit();
            assert!(e.field().starts_with("__"));
        }
        assert_eq!(seen, u16::MAX);
    }
}
