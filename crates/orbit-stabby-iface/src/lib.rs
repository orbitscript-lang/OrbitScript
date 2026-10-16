//! SPIKE: the boundary between liborbit and programs, described with
//! `stabby`: one exported constructor whose signature report is checked on
//! first call, and three `#[stabby::stabby]` traits used as trait objects.

use stabby::{
    boxed::Box, dynptr, result::Result, slice::Slice, str::Str, string::String, vec::Vec,
};

/// A value at the boundary (`orbit_api_types::Val`).
#[stabby::stabby]
#[repr(u8)]
#[derive(Debug, Clone)]
pub enum SVal {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Ref(u32),
}

pub type Vals = Result<Vec<SVal>, String>;

/// What a native sees.
#[stabby::stabby(checked)]
pub trait NativeCtx {
    extern "C" fn nargs(&self) -> usize;
    extern "C" fn arg(&mut self, i: usize) -> SVal;
    extern "C" fn push(&mut self, v: SVal);
    extern "C" fn call<'a>(&mut self, f: SVal, args: Slice<'a, SVal>) -> Vals;
}

pub type CtxMut<'a> = dynptr!(&'a mut dyn NativeCtx);

/// A program's native, as an object the library holds.
#[stabby::stabby(checked)]
pub trait HostFn {
    extern "C" fn run<'a>(&self, ctx: CtxMut<'a>) -> Result<usize, String>;
}

pub type HostFnBox = dynptr!(Box<dyn HostFn>);

/// A Lua state.
#[stabby::stabby(checked)]
pub trait Engine {
    extern "C" fn load<'a>(&mut self, src: Slice<'a, u8>, name: Str<'a>) -> Result<SVal, String>;
    extern "C" fn call<'a>(&mut self, f: SVal, args: Slice<'a, SVal>) -> Vals;
    extern "C" fn set_global<'a>(&mut self, name: Str<'a>, v: SVal);
    extern "C" fn global<'a>(&mut self, name: Str<'a>) -> SVal;
    extern "C" fn new_table(&mut self) -> SVal;
    extern "C" fn table_set(&mut self, t: SVal, k: SVal, v: SVal);
    extern "C" fn release(&mut self, r: u32);
    extern "C" fn tostring(&mut self, v: SVal) -> Vec<u8>;
    extern "C" fn register<'a>(&mut self, name: Str<'a>, f: HostFnBox);
}

pub type EngineBox = dynptr!(Box<dyn Engine>);
