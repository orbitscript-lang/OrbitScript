//! Metamethod events (Lua's `TMS`).

use crate::value::StrRef;
use orbit_gc::Gc;

/// A metamethod event. The order matches Lua 5.5's `ltm.h`: the first six have a
/// per-metatable absence cache ([`Tm::is_fast`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Tm {
    Index,
    NewIndex,
    Gc,
    Mode,
    Len,
    Eq,
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Unm,
    BNot,
    Lt,
    Le,
    Concat,
    Call,
    Close,
}

impl Tm {
    /// Every event, in order.
    pub const ALL: [Tm; 25] = [
        Tm::Index,
        Tm::NewIndex,
        Tm::Gc,
        Tm::Mode,
        Tm::Len,
        Tm::Eq,
        Tm::Add,
        Tm::Sub,
        Tm::Mul,
        Tm::Mod,
        Tm::Pow,
        Tm::Div,
        Tm::IDiv,
        Tm::BAnd,
        Tm::BOr,
        Tm::BXor,
        Tm::Shl,
        Tm::Shr,
        Tm::Unm,
        Tm::BNot,
        Tm::Lt,
        Tm::Le,
        Tm::Concat,
        Tm::Call,
        Tm::Close,
    ];

    /// The metatable field name, such as `__index`.
    pub const fn name(self) -> &'static str {
        match self {
            Tm::Index => "__index",
            Tm::NewIndex => "__newindex",
            Tm::Gc => "__gc",
            Tm::Mode => "__mode",
            Tm::Len => "__len",
            Tm::Eq => "__eq",
            Tm::Add => "__add",
            Tm::Sub => "__sub",
            Tm::Mul => "__mul",
            Tm::Mod => "__mod",
            Tm::Pow => "__pow",
            Tm::Div => "__div",
            Tm::IDiv => "__idiv",
            Tm::BAnd => "__band",
            Tm::BOr => "__bor",
            Tm::BXor => "__bxor",
            Tm::Shl => "__shl",
            Tm::Shr => "__shr",
            Tm::Unm => "__unm",
            Tm::BNot => "__bnot",
            Tm::Lt => "__lt",
            Tm::Le => "__le",
            Tm::Concat => "__concat",
            Tm::Call => "__call",
            Tm::Close => "__close",
        }
    }

    /// Whether absence of this event is cached in the metatable's flags
    /// (`TM_INDEX` through `TM_EQ`).
    #[inline(always)]
    pub const fn is_fast(self) -> bool {
        (self as u8) <= Tm::Eq as u8
    }

    /// The interned name string. Every heap creates the event names first, in this
    /// order, and keeps them alive, so the handle is a constant.
    #[inline(always)]
    pub const fn str_ref(self) -> StrRef {
        Gc::from_index(self as u32)
    }
}
