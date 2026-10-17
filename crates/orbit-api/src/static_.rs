//! The `static` backend: the runtime linked into the program.

use crate::engine::Engine;
use crate::raw::{RawNative, RawVal};
use crate::{Error, Options};

pub use crate::engine::tools;

pub type Interrupt = orbit::Interrupt;

pub struct State(Engine);

impl State {
    pub fn new(options: &Options) -> Result<State, Error> {
        Engine::new(options).map(State)
    }
    pub fn release(&mut self, id: u32) {
        self.0.release(id)
    }
    pub fn load(&mut self, chunk: &[u8], source: &[u8]) -> Result<RawVal, Error> {
        self.0.load(chunk, source)
    }
    pub fn load_file(&mut self, path: Option<&str>) -> Result<RawVal, Error> {
        self.0.load_file(path)
    }
    pub fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error> {
        self.0.call(f, args)
    }
    pub fn call_with_traceback(
        &mut self,
        f: &RawVal,
        args: &[RawVal],
    ) -> Result<Vec<RawVal>, Error> {
        self.0.call_with_traceback(f, args)
    }
    pub fn global(&mut self, name: &str) -> RawVal {
        self.0.global(name)
    }
    pub fn set_global(&mut self, name: &str, v: &RawVal) {
        self.0.set_global(name, v)
    }
    pub fn new_table(&mut self) -> RawVal {
        self.0.new_table()
    }
    pub fn get(&mut self, t: &RawVal, k: &RawVal) -> RawVal {
        self.0.get(t, k)
    }
    pub fn set(&mut self, t: &RawVal, k: &RawVal, v: &RawVal) -> Result<(), Error> {
        self.0.set(t, k, v)
    }
    pub fn len(&mut self, v: &RawVal) -> i64 {
        self.0.len(v)
    }
    pub fn tostring(&mut self, v: &RawVal) -> Result<Vec<u8>, Error> {
        self.0.tostring(v)
    }
    pub fn function(&mut self, name: &str, f: RawNative) -> RawVal {
        self.0.function(name, f)
    }
    pub fn set_warnings(&mut self, on: bool) {
        self.0.set_warnings(on)
    }
    pub fn interrupt(&mut self) -> Interrupt {
        self.0.interrupt()
    }
    pub fn flush(&mut self) {
        self.0.flush()
    }
}
