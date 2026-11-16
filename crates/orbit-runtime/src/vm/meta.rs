//! Metatables and metamethods ([VM semantics]).
//!
//! Slow paths never run Lua code. When one needs a metamethod that is a
//! function, it returns [`Exc::Meta`] with the call to make; the cold
//! `raise` path turns that into a call with a meta frame below it
//! ([`FrameKind::Meta`]) that finishes the instruction when the call
//! returns. Chains through tables (`__index = other_table`) are followed
//! here without calls.
//!
//! [VM semantics]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm-semantics.md#metamethods

use crate::object::OrbitString;

use crate::locale::Locale;
use crate::object::table;
use crate::object::{Gc, Interner, LuaHeap, LuaStr, Table, Tag, Value};

use crate::vm::VmState;
use crate::vm::ops::Exc;

/// How far `__index` and `__newindex` chains are followed before giving up
/// (the reference's limit).
pub(crate) const MAX_CHAIN: usize = 2000;

pub(crate) use crate::object::MetaField;

/// The interned metatable field names, rooted by the runtime, so a metatable lookup
/// is a handle-keyed probe; and the type names, so `type` needs no string
/// of its own.
#[derive(Debug)]
pub(crate) struct MetaFieldNames {
    /// The field names in [`MetaField::ALL`] order, then [`TYPE_NAMES`].
    names: Box<[Gc<LuaStr>]>,
}

/// The names [`Value::type_name`] gives, in the order
/// [`MetaFieldNames::type_name`] finds them.
const TYPE_NAMES: [&str; 8] = [
    "nil", "boolean", "number", "string", "table", "function", "thread", "userdata",
];

impl MetaFieldNames {
    pub(crate) fn new(heap: &mut LuaHeap, strings: &mut Interner) -> MetaFieldNames {
        let fields = MetaField::ALL.iter().map(|e| e.field());
        MetaFieldNames {
            names: fields
                .chain(TYPE_NAMES)
                .map(|n| strings.new_str(heap, n.as_bytes()))
                .collect(),
        }
    }

    #[inline]
    pub(crate) fn name(&self, e: MetaField) -> Gc<LuaStr> {
        self.names[e as usize]
    }

    /// The interned [`Value::type_name`] of `v`.
    #[inline]
    pub(crate) fn type_name(&self, v: Value) -> Gc<LuaStr> {
        let i = match v {
            Value::Nil => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) => 2,
            Value::Str(_) => 3,
            Value::Table(_) => 4,
            Value::Closure(_) | Value::Native(_) => 5,
            Value::Thread(_) => 6,
            Value::UserData(_) | Value::LightUserData(_) => 7,
        };
        self.names[MetaField::ALL.len() + i]
    }

    /// Every name, for the collector's roots.
    pub(crate) fn all(&self) -> &[Gc<LuaStr>] {
        &self.names
    }
}

/// What metamethod lookups need: the field names and the metatables of
/// the types whose values don't carry their own (one slot per tag); and
/// the locale numbers and string comparisons follow. Kept apart from the
/// stack so slow paths can borrow it while the dispatch loop holds the
/// register window.
#[derive(Debug)]
pub(crate) struct Metas {
    pub(crate) field_names: MetaFieldNames,
    pub(crate) type_meta: [Option<Gc<Table>>; 11],
    pub(crate) locale: Locale,
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
    pub(crate) fn type_name(&self, heap: &LuaHeap, v: Value) -> OrbitString {
        if let Value::Table(_) | Value::UserData(_) = v {
            if let Value::Str(s) = self.metamethod(heap, v, MetaField::Name) {
                return heap.get(s).as_bytes().into();
            }
        }
        v.type_name().into()
    }
}

/// A metamethod call a slow path needs before its instruction can finish.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetaCall {
    pub(crate) f: Value,
    pub(crate) args: [Value; 3],
    pub(crate) nargs: u8,
    pub(crate) field: MetaField,
    /// Instruction-specific state: for `Concat`, how many values are left;
    /// for a comparison, [`NEGATE`] when the result must be negated.
    pub(crate) aux: u8,
}

/// [`MetaCall::aux`] of a comparison answered by negating its handler's
/// result (Lua 5.4's `__le` through `__lt`, ADR-14).
pub(crate) const NEGATE: u8 = 1;

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

/// How a Lua frame entered as a metamethod handler finishes the instruction
/// that asked for it when it returns, without a meta frame below it: the
/// common requests, whose handler is a Lua function. A return from that
/// frame applies it; the frame's `want` is [`META_WANT`] so that the
/// one-result fast return passes it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaReturn {
    /// `R[dst] = result`.
    Store { dst: u8, field: MetaField },
    /// Jump by `off` when the result's truthiness equals `want`.
    Branch {
        want: bool,
        field: MetaField,
        off: i32,
    },
    /// Nothing (`__newindex`).
    Discard { field: MetaField },
}

impl MetaReturn {
    /// The metamethod the frame runs, for its name in tracebacks.
    pub(crate) fn field(self) -> MetaField {
        match self {
            MetaReturn::Store { field, .. }
            | MetaReturn::Branch { field, .. }
            | MetaReturn::Discard { field } => field,
        }
    }

    /// `action` of a `field` handler, when it is one of these.
    pub(crate) fn of(action: MetaAction, field: MetaField) -> Option<MetaReturn> {
        match action {
            MetaAction::Store { dst } => Some(MetaReturn::Store { dst, field }),
            MetaAction::Branch { want, off } => Some(MetaReturn::Branch { want, field, off }),
            MetaAction::Discard => Some(MetaReturn::Discard { field }),
            _ => None,
        }
    }
}

/// The `want` of a metamethod handler's Lua frame ([`MetaReturn`]): not
/// one result, so the one-result fast return leaves the frame to the
/// general return, which finds its [`MetaReturn`]. A call that really
/// wants this many results is told apart by the frame's kind.
pub(crate) const META_WANT: u8 = 254;

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
    /// `Cast` to a string through `__tostring`: `R[dst] = result`, which
    /// must be a string or a number (converted).
    ToStr { dst: u8 },
    /// A `__close` handler run while an error unwinds: raise the error
    /// again, past its message handler. Its value waits in the frame's
    /// `func` slot and its traceback, if it has one, in the slot above.
    Unwind { cause: crate::vm::Cause },
}

impl VmState {
    /// The metatable of `v`, if any.
    #[inline]
    pub(crate) fn metatable(&self, heap: &LuaHeap, v: Value) -> Option<Gc<Table>> {
        self.metas.metatable(heap, v)
    }

    /// Field `e` of `v`'s metatable (raw), or nil.
    #[inline]
    pub(crate) fn metamethod(&self, heap: &LuaHeap, v: Value, e: MetaField) -> Value {
        self.metas.metamethod(heap, v, e)
    }

    /// The metatable shared by every value of `tag`'s type (strings,
    /// numbers, booleans, nil, functions); tables and userdata have their
    /// own.
    pub(crate) fn type_meta(&self, tag: Tag) -> Option<Gc<Table>> {
        self.metas.type_meta[tag as usize]
    }

    /// Set the shared metatable of `tag`'s type. Integers and floats share
    /// one, as do Lua closures and natives.
    pub(crate) fn set_type_meta(&mut self, tag: Tag, meta: Option<Gc<Table>>) {
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
    pub(crate) fn field_name(&self, e: MetaField) -> Gc<LuaStr> {
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

/// [`index`] when `obj`, if it is a table, was already looked `key` up in
/// raw and missed (the dispatch loop's index requests): starts at its
/// metatable instead of looking the key up again.
#[cold]
#[inline(never)]
pub(crate) fn index_after_miss(
    heap: &LuaHeap,
    rt: &Metas,
    obj: Value,
    key: Value,
) -> Result<Value, Exc> {
    let Value::Table(t) = obj else {
        return index(heap, rt, obj, key);
    };
    let Some(mt) = heap.get(t).meta else {
        return Ok(Value::Nil);
    };
    let h = heap.get(mt).meta_field(
        heap,
        MetaField::Index,
        rt.field_names.name(MetaField::Index),
    );
    if h.is_nil() {
        return Ok(Value::Nil);
    }
    if h.is_function() {
        return Err(Exc::Meta(MetaCall::new(h, MetaField::Index, &[obj, key])));
    }
    index(heap, rt, h, key)
}

/// [`index`] for table `obj`, whose raw lookup of string key `key` found
/// nothing and whose metatable is `mt`. Walks a chain of `__index` tables
/// with the key's known hash; anything else goes to [`index`].
pub(crate) fn index_str_miss(
    heap: &LuaHeap,
    rt: &Metas,
    obj: Gc<Table>,
    mt: Gc<Table>,
    key: &table::Key,
) -> Result<Value, Exc> {
    let name = rt.field_names.name(MetaField::Index);
    let (mut o, mut mt) = (obj, mt);
    // `obj` itself was the chain's first step.
    for _ in 1..MAX_CHAIN {
        let h = heap.get(mt).meta_field(heap, MetaField::Index, name);
        let Value::Table(t) = h else {
            if h.is_nil() {
                return Ok(Value::Nil);
            }
            if h.is_function() {
                return Err(Exc::Meta(MetaCall::new(
                    h,
                    MetaField::Index,
                    &[Value::Table(o), key.value()],
                )));
            }
            return index(heap, rt, h, key.value());
        };
        let tab = heap.get(t);
        if let Some(slot) = tab.find_str_key(heap, key) {
            let v = tab.slot_value(slot);
            if !v.is_nil() {
                return Ok(v);
            }
        }
        match tab.meta {
            Some(m) => (o, mt) = (t, m),
            None => return Ok(Value::Nil),
        }
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
        bytes_msg!["attempt to index a ", rt.type_name(heap, v), " value"],
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
