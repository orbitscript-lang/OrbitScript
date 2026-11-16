//! The objects a running Lua program manipulates, on top of `orbit-gc`:
//! [`Value`], strings, tables, closures, cells and prototypes, plus number
//! parsing and formatting ([VM design], ADR-1, ADR-2, ADR-5).
//!
//! Everything lives in one [`LuaHeap`]. Handles ([`Gc`]) are plain indexes,
//! so reading an object takes the heap: `heap.get(t)`. Operations that need
//! to read other objects while changing one (a table store has to hash its
//! string key) are split into a read phase on `&LuaHeap` and a write through
//! [`Heap::update`](orbit_gc::Heap::update), which also runs the write
//! barrier.
//!
//! [VM design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm.md

pub(crate) mod func;
pub(crate) mod gc;
pub(crate) mod mem;
pub(crate) mod meta;
pub mod number;
pub(crate) mod orbit_str;
pub(crate) mod string;
pub(crate) mod table;
pub(crate) mod thread;
pub(crate) mod userdata;
pub(crate) mod value;

pub(crate) use func::{Cell, Closure, FieldCache, Proto, ProtoCode};
pub use mem::NoMemory;
pub(crate) use meta::MetaField;
pub use orbit_gc::Gc;
pub(crate) use orbit_gc::Heap;
pub use orbit_gc::Verify;
pub use orbit_str::{OrbitStr, OrbitString};
pub(crate) use string::{CStrings, HashSeed, Interner, LuaStr};
pub use table::Table;
pub(crate) use thread::Thread;
pub(crate) use userdata::UserData;
pub use value::Value;
pub(crate) use value::{LightUserData, NativeId, Payload, Tag};

orbit_gc::space! {
    /// Every object type of the Lua heap.
    pub(crate) struct LuaSpace {
        strings: LuaStr,
        tables: Table,
        closures: Closure,
        cells: Cell,
        protos: Proto,
        userdata: UserData,
        threads: Thread,
    }
}

/// The Lua heap.
pub(crate) type LuaHeap = Heap<LuaSpace>;
