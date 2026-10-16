//! SPIKE: one embedding API, four ways to reach the runtime. `static` links
//! it into the program; `table`, `abi` and `stabby` reach `liborbit` through
//! a C ABI and wrap it back into this same safe API.

pub use orbit_api_types::{Ctx, NativeFn, Val};

#[cfg(feature = "static")]
#[path = "static_.rs"]
mod imp;
#[cfg(feature = "table")]
#[path = "table.rs"]
mod imp;
#[cfg(feature = "abi")]
#[path = "abi.rs"]
mod imp;
#[cfg(feature = "stabby")]
#[path = "stabby_.rs"]
mod imp;

/// A Lua state with every library open.
pub struct State(imp::State);

impl State {
    pub fn new(ffi: bool) -> Result<State, String> {
        imp::State::new(ffi).map(State)
    }
    /// Compile a chunk; `name` as `loadfile` takes it (`@file`, `=name`).
    pub fn load(&mut self, src: &[u8], name: &str) -> Result<Val, String> {
        self.0.load(src, name)
    }
    pub fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        self.0.call(f, args)
    }
    pub fn set_global(&mut self, name: &str, v: &Val) {
        self.0.set_global(name, v)
    }
    pub fn global(&mut self, name: &str) -> Val {
        self.0.global(name)
    }
    pub fn new_table(&mut self) -> Val {
        self.0.new_table()
    }
    pub fn table_set(&mut self, t: &Val, k: &Val, v: &Val) {
        self.0.table_set(t, k, v)
    }
    pub fn release(&mut self, r: u32) {
        self.0.release(r)
    }
    pub fn tostring(&mut self, v: &Val) -> Vec<u8> {
        self.0.tostring(v)
    }
    /// Make `f` the global `name`.
    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        self.0.register(name, f)
    }
}
