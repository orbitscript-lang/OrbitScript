//! Values, strings, tables and the other Lua object types, built on the `orbit-gc`
//! collector (stream B; ADR-1, ADR-2, ADR-5).
//!
//! - [`Value`]: the 16-byte `Copy` representation of every Lua value.
//! - [`OrbitString`] and [`OrbitStr`]: the byte-string types embedders and native
//!   functions use, with easy conversion to and from Rust strings.
//! - [`LuaStr`]: a string inside the GC heap, with short strings stored inline.
//! - [`Table`]: Lua's hybrid array/hash table, with a split-tag array part and its own
//!   24-byte-node hash part.
//! - [`object`]: closures, upvalues, userdata, prototypes and coroutines.

pub mod heap;
pub mod object;
mod orbit_string;
pub mod string;
pub mod table;
pub mod tm;
pub mod value;

pub use heap::{Heap, LuaSpace};
pub use orbit_gc::Gc;
pub use orbit_string::{OrbitStr, OrbitString};
pub use string::LuaStr;
pub use table::Table;
pub use tm::Tm;
pub use value::{NativeId, Type, Value};
