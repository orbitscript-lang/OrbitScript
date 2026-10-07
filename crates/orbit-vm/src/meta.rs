//! Metatables and metamethods ([VM semantics]).
//!
//! Slow paths never run Lua code. When one needs a metamethod that is a
//! function, it returns [`Exc::Meta`] with the call to make; the cold
//! `raise` path turns that into a call with a meta frame below it
//! ([`FrameKind::Meta`]) that finishes the instruction when the call
//! returns. Chains through tables (`__index = other_table`) are followed
//! here without calls.
//!
//! [VM semantics]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm-semantics.md#metamethods

use orbit_core::table;
use orbit_core::{Gc, Interner, LuaHeap, LuaStr, Table, Tag, Value};

use crate::Runtime;
use crate::ops::Exc;

/// How far `__index` and `__newindex` chains are followed before giving up
/// (the reference's limit).
pub(crate) const MAX_CHAIN: usize = 2000;

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

events! {
    Index = "__index",
    NewIndex = "__newindex",
    Gc = "__gc",
    Mode = "__mode",
    Len = "__len",
    Eq = "__eq",
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
    Unm = "__unm",
    BNot = "__bnot",
    Lt = "__lt",
    Le = "__le",
    Concat = "__concat",
    Call = "__call",
    Close = "__close",
    ToString = "__tostring",
    Name = "__name",
    Metatable = "__metatable",
    Pairs = "__pairs",
}

impl Event {
    /// The name without the underscores, as messages and tracebacks print
    /// it (`metamethod 'index'`).
    pub fn short_name(self) -> &'static str {
        &self.field()[2..]
    }
}

/// The interned event names, rooted by the runtime, so a metatable lookup
/// is a handle-keyed probe.
#[derive(Debug)]
pub(crate) struct Events {
    names: Box<[Gc<LuaStr>]>,
}

impl Events {
    pub(crate) fn new(heap: &mut LuaHeap, strings: &mut Interner) -> Events {
        Events {
            names: Event::ALL
                .iter()
                .map(|e| strings.new_str(heap, e.field().as_bytes()))
                .collect(),
        }
    }

    #[inline]
    pub(crate) fn name(&self, e: Event) -> Gc<LuaStr> {
        self.names[e as usize]
    }

    pub(crate) fn all(&self) -> &[Gc<LuaStr>] {
        &self.names
    }
}

/// What metamethod lookups need: the event names and the metatables of
/// the types whose values don't carry their own (one slot per tag). Kept
/// apart from the stack so slow paths can borrow it while the dispatch
/// loop holds the register window.
#[derive(Debug)]
pub(crate) struct Metas {
    pub(crate) events: Events,
    pub(crate) type_meta: [Option<Gc<Table>>; 11],
}

impl Metas {
    /// The metatable of `v`, if any.
    #[inline]
    pub(crate) fn metatable(&self, heap: &LuaHeap, v: Value) -> Option<Gc<Table>> {
        match v {
            Value::Table(t) => heap.get(t).meta,
            Value::UserData(u) => heap.get(u).meta,
            v => self.type_meta[v.tag() as usize],
        }
    }

    /// Field `e` of `v`'s metatable (raw), or nil.
    #[inline]
    pub(crate) fn metamethod(&self, heap: &LuaHeap, v: Value, e: Event) -> Value {
        match self.metatable(heap, v) {
            Some(mt) => heap.get(mt).get_str(heap, self.events.name(e)),
            None => Value::Nil,
        }
    }
}

/// A metamethod call a slow path needs before its instruction can finish.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetaCall {
    pub f: Value,
    pub args: [Value; 3],
    pub nargs: u8,
    pub event: Event,
    /// Instruction-specific state: for `Concat`, how many values are left.
    pub aux: u8,
}

impl MetaCall {
    pub(crate) fn new(f: Value, event: Event, args: &[Value]) -> MetaCall {
        let mut a = [Value::Nil; 3];
        a[..args.len()].copy_from_slice(args);
        MetaCall {
            f,
            args: a,
            nargs: args.len() as u8,
            event,
            aux: 0,
        }
    }
}

/// How to finish an instruction once its metamethod returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaAction {
    /// `R[dst] = result`.
    Store { dst: u8 },
    /// Jump by `off` when the result's truthiness equals `want`.
    Branch { want: bool, off: i32 },
    /// Nothing (`__newindex`).
    Discard,
    /// A `..` with `left` values still to reduce: the result replaces the
    /// last two of them.
    Concat { dst: u8, first: u8, left: u8 },
    /// `Close`: restore `rt.top` (the open-ended results the close must
    /// keep) and run the instruction again for the next variable.
    Close { top: u32 },
    /// A `__close` handler run while an error unwinds: raise the error
    /// (in the frame's `func` slot) again. `handled`: it has been through
    /// its message handler.
    Unwind { handled: bool },
}

impl Runtime {
    /// The metatable of `v`, if any.
    #[inline]
    pub fn metatable(&self, heap: &LuaHeap, v: Value) -> Option<Gc<Table>> {
        self.metas.metatable(heap, v)
    }

    /// Field `e` of `v`'s metatable (raw), or nil.
    #[inline]
    pub fn metamethod(&self, heap: &LuaHeap, v: Value, e: Event) -> Value {
        self.metas.metamethod(heap, v, e)
    }

    /// The metatable shared by every value of `tag`'s type (strings,
    /// numbers, booleans, nil, functions); tables and userdata have their
    /// own.
    pub fn type_meta(&self, tag: Tag) -> Option<Gc<Table>> {
        self.metas.type_meta[tag as usize]
    }

    /// Set the shared metatable of `tag`'s type. Integers and floats share
    /// one, as do Lua closures and natives.
    pub fn set_type_meta(&mut self, tag: Tag, meta: Option<Gc<Table>>) {
        let slots: &[Tag] = match tag {
            Tag::Int | Tag::Float => &[Tag::Int, Tag::Float],
            Tag::Closure | Tag::Native => &[Tag::Closure, Tag::Native],
            _ => std::slice::from_ref(&tag),
        };
        for &t in slots {
            self.metas.type_meta[t as usize] = meta;
        }
    }

    /// The interned name of event `e`.
    pub fn event_name(&self, e: Event) -> Gc<LuaStr> {
        self.metas.events.name(e)
    }
}

/// `obj[key]` with `__index`, once a raw lookup has missed (or `obj` isn't
/// a table). `obj` is the instruction's operand 0, for error messages.
#[cold]
#[inline(never)]
pub(crate) fn index(heap: &LuaHeap, rt: &Metas, obj: Value, key: Value) -> Result<Value, Exc> {
    let mut o = obj;
    for _ in 0..MAX_CHAIN {
        let h = match o {
            Value::Table(t) => {
                let tab = heap.get(t);
                let v = tab.get(heap, key);
                if !v.is_nil() {
                    return Ok(v);
                }
                match tab.meta {
                    Some(mt) => heap.get(mt).get_str(heap, rt.events.name(Event::Index)),
                    None => return Ok(Value::Nil),
                }
            }
            _ => {
                let h = rt.metamethod(heap, o, Event::Index);
                if h.is_nil() {
                    return Err(index_type_error(o, o.same(obj)));
                }
                h
            }
        };
        if h.is_nil() {
            return Ok(Value::Nil);
        }
        if h.is_function() {
            return Err(Exc::Meta(MetaCall::new(h, Event::Index, &[o, key])));
        }
        o = h;
    }
    Err(Exc::Rt(
        "'__index' chain too long; possible loop".into(),
        None,
    ))
}

/// `obj[key] = v` with `__newindex`.
#[cold]
#[inline(never)]
pub(crate) fn new_index(
    heap: &mut LuaHeap,
    rt: &Metas,
    obj: Value,
    key: Value,
    v: Value,
) -> Result<(), Exc> {
    let mut o = obj;
    for _ in 0..MAX_CHAIN {
        let h = match o {
            Value::Table(t) => {
                let tab = heap.get(t);
                let h = match tab.meta {
                    Some(mt) if tab.get(heap, key).is_nil() => {
                        heap.get(mt).get_str(heap, rt.events.name(Event::NewIndex))
                    }
                    _ => Value::Nil,
                };
                if h.is_nil() {
                    return table::set(heap, t, key, v)
                        .map_err(|e| Exc::Rt(e.message().into(), None));
                }
                h
            }
            _ => {
                let h = rt.metamethod(heap, o, Event::NewIndex);
                if h.is_nil() {
                    return Err(index_type_error(o, o.same(obj)));
                }
                h
            }
        };
        if h.is_function() {
            return Err(Exc::Meta(MetaCall::new(h, Event::NewIndex, &[o, key, v])));
        }
        o = h;
    }
    Err(Exc::Rt(
        "'__newindex' chain too long; possible loop".into(),
        None,
    ))
}

/// Indexing a value that can't be indexed. Only the instruction's own
/// operand has a name to report.
pub(crate) fn index_type_error(v: Value, is_operand: bool) -> Exc {
    Exc::Rt(
        format!("attempt to index a {} value", v.type_name()),
        is_operand.then_some(0),
    )
}

/// The handler for binary event `e` on `a` and `b`: `a`'s, else `b`'s.
#[inline]
pub(crate) fn binary_handler(heap: &LuaHeap, rt: &Metas, e: Event, a: Value, b: Value) -> Value {
    let h = rt.metamethod(heap, a, e);
    if h.is_nil() {
        rt.metamethod(heap, b, e)
    } else {
        h
    }
}
