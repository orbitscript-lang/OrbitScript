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
//! [VM design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md

pub mod func;
pub mod number;
pub mod string;
pub mod table;
pub mod value;

pub use func::{Cell, Closure, Proto};
pub use orbit_gc::{Gc, Heap};
pub use string::{Interner, LuaStr, OrbitStr, OrbitString};
pub use table::Table;
pub use value::{NativeId, Value};

orbit_gc::space! {
    /// Every object type of the Lua heap.
    pub struct LuaSpace {
        strings: LuaStr,
        tables: Table,
        closures: Closure,
        cells: Cell,
        protos: Proto,
    }
}

/// The Lua heap.
pub type LuaHeap = Heap<LuaSpace>;
