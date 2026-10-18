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

use std::borrow::Cow;

use crate::object::table;
use crate::object::{Gc, Interner, LuaHeap, LuaStr, Table, Tag, Value};

use crate::vm::Runtime;
use crate::vm::ops::Exc;

/// How far `__index` and `__newindex` chains are followed before giving up
/// (the reference's limit).
pub(crate) const MAX_CHAIN: usize = 2000;

pub use crate::object::MetaField;

/// The interned metatable field names, rooted by the runtime, so a metatable lookup
/// is a handle-keyed probe.
#[derive(Debug)]
pub(crate) struct MetaFieldNames {
    names: Box<[Gc<LuaStr>]>,
}

impl MetaFieldNames {
    pub(crate) fn new(heap: &mut LuaHeap, strings: &mut Interner) -> MetaFieldNames {
        MetaFieldNames {
            names: MetaField::ALL
                .iter()
                .map(|e| strings.new_str(heap, e.field().as_bytes()))
                .collect(),
        }
    }

    #[inline]
    pub(crate) fn name(&self, e: MetaField) -> Gc<LuaStr> {
        self.names[e as usize]
    }

    pub(crate) fn all(&self) -> &[Gc<LuaStr>] {
        &self.names
    }
}

/// What metamethod lookups need: the field names and the metatables of
/// the types whose values don't carry their own (one slot per tag). Kept
/// apart from the stack so slow paths can borrow it while the dispatch
/// loop holds the register window.
#[derive(Debug)]
pub(crate) struct Metas {
    pub(crate) field_names: MetaFieldNames,
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
    pub(crate) fn metamethod(&self, heap: &LuaHeap, v: Value, e: MetaField) -> Value {
        match self.metatable(heap, v) {
            Some(mt) => heap.get(mt).meta_field(heap, e, self.field_names.name(e)),
            None => Value::Nil,
        }
    }

    /// The type name error messages report for `v`: a table's or full
    /// userdata's `__name`, when that is a string, else its type name.
    /// (lua5.5 ignores `__name` in the shared metatables of other types.)
    #[cold]
    pub(crate) fn type_name(&self, heap: &LuaHeap, v: Value) -> Cow<'static, str> {
        if let Value::Table(_) | Value::UserData(_) = v
            && let Value::Str(s) = self.metamethod(heap, v, MetaField::Name)
        {
            return Cow::Owned(String::from_utf8_lossy(heap.get(s).as_bytes()).into_owned());
        }
        Cow::Borrowed(v.type_name())
    }
}

/// A metamethod call a slow path needs before its instruction can finish.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetaCall {
    pub f: Value,
    pub args: [Value; 3],
    pub nargs: u8,
    pub field: MetaField,
    /// Instruction-specific state: for `Concat`, how many values are left.
    pub aux: u8,
}

impl MetaCall {
    pub(crate) fn new(f: Value, field: MetaField, args: &[Value]) -> MetaCall {
        let mut a = [Value::Nil; 3];
        a[..args.len()].copy_from_slice(args);
        MetaCall {
            f,
            args: a,
            nargs: args.len() as u8,
            field,
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
    pub fn metamethod(&self, heap: &LuaHeap, v: Value, e: MetaField) -> Value {
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

    /// The interned name of metatable field `e`.
    pub fn field_name(&self, e: MetaField) -> Gc<LuaStr> {
        self.metas.field_names.name(e)
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
                    Some(mt) => heap.get(mt).meta_field(
                        heap,
                        MetaField::Index,
                        rt.field_names.name(MetaField::Index),
                    ),
                    None => return Ok(Value::Nil),
                }
            }
            _ => {
                let h = rt.metamethod(heap, o, MetaField::Index);
                if h.is_nil() {
                    return Err(index_type_error(heap, rt, o, o.same(obj)));
                }
                h
            }
        };
        if h.is_nil() {
            return Ok(Value::Nil);
        }
        if h.is_function() {
            return Err(Exc::Meta(MetaCall::new(h, MetaField::Index, &[o, key])));
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
                        let e = MetaField::NewIndex;
                        heap.get(mt).meta_field(heap, e, rt.field_names.name(e))
                    }
                    _ => Value::Nil,
                };
                if h.is_nil() {
                    return table::set(heap, t, key, v).map_err(Exc::from);
                }
                h
            }
            _ => {
                let h = rt.metamethod(heap, o, MetaField::NewIndex);
                if h.is_nil() {
                    return Err(index_type_error(heap, rt, o, o.same(obj)));
                }
                h
            }
        };
        if h.is_function() {
            return Err(Exc::Meta(MetaCall::new(
                h,
                MetaField::NewIndex,
                &[o, key, v],
            )));
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
pub(crate) fn index_type_error(heap: &LuaHeap, rt: &Metas, v: Value, is_operand: bool) -> Exc {
    Exc::Rt(
        format!("attempt to index a {} value", rt.type_name(heap, v)),
        is_operand.then_some(0),
    )
}

/// The handler for binary operator field `e` on `a` and `b`: `a`'s, else `b`'s.
#[inline]
pub(crate) fn binary_handler(
    heap: &LuaHeap,
    rt: &Metas,
    e: MetaField,
    a: Value,
    b: Value,
) -> Value {
    let h = rt.metamethod(heap, a, e);
    if h.is_nil() {
        rt.metamethod(heap, b, e)
    } else {
        h
    }
}
