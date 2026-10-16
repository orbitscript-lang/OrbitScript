//! The runtime linked into the program.
use orbit_api_core::Engine;
use orbit_api_types::{NativeFn, Val};

pub struct State(Engine);

impl State {
    pub fn new(ffi: bool) -> Result<State, String> {
        Ok(State(Engine::new(ffi)))
    }
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
    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        self.0.register(name, f)
    }
}
