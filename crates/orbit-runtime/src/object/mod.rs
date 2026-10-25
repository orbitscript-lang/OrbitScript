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

pub mod func;
pub mod gc;
pub mod mem;
pub mod meta;
pub mod number;
pub mod orbit_str;
pub mod string;
pub mod table;
pub mod thread;
pub mod userdata;
pub mod value;

pub use func::{Cell, Closure, FieldCache, Proto, ProtoCode};
pub use mem::NoMemory;
pub use meta::MetaField;
pub use orbit_gc::{Gc, Heap};
pub use orbit_str::{OrbitStr, OrbitString};
pub use string::{Interner, LuaStr};
pub use table::Table;
pub use thread::{Thread, ThreadState};
pub use userdata::UserData;
pub use value::{LightUserData, NativeId, Payload, Tag, Value};

orbit_gc::space! {
    /// Every object type of the Lua heap.
    pub struct LuaSpace {
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
pub type LuaHeap = Heap<LuaSpace>;
