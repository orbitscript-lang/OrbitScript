//! The boundary between `liborbit` and Rust programs that use it through
//! `orbit-api`'s `shared` backend (ADR-10), described with `stabby`.
//!
//! The library exports one function, [`OPEN`], which takes the program's
//! [`INTERFACE_HASH`] and returns a [`Library`] object or refuses. Values
//! cross as [`AValue`]: scalars and strings by copy, references as numbers
//! the library keeps alive until released. Everything else is a
//! `#[stabby::stabby]` trait used as a trait object.
//!
//! Changing anything in this file changes [`INTERFACE_HASH`], so a program
//! and a library built from different versions of it refuse each other.

// stabby's trait macro needs every reference's lifetime named.
#![allow(clippy::needless_lifetimes)]

use stabby::{
    boxed::Box, dynptr, result::Result, slice::Slice, str::Str, string::String, tuple::Tuple2,
    vec::Vec,
};

include!(concat!(env!("OUT_DIR"), "/hash.rs"));

/// The name of the library's one exported function.
pub const OPEN: &str = "orbit_abi_open";

/// A value at the boundary.
#[stabby::stabby]
#[repr(u8)]
#[derive(Debug, Clone)]
pub enum AValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Table(u32),
    Function(u32),
    Userdata(u32),
    Thread(u32),
}

/// An error at the boundary: a syntax error (`syntax`) or a runtime one,
/// with a traceback when `has_traceback`.
#[stabby::stabby]
#[derive(Debug, Clone)]
pub struct AError {
    pub message: String,
    pub traceback: String,
    pub syntax: bool,
    pub has_traceback: bool,
}

pub type AResult<T> = Result<T, AError>;

/// `Options` at the boundary.
#[stabby::stabby]
#[derive(Debug, Clone, Copy)]
pub struct AOptions {
    /// [`OPT_IGNORE_ENV`] and the other `OPT_` bits.
    pub flags: u32,
    /// The heap limit in bytes, or `u64::MAX` for none.
    pub memory_limit: u64,
}

pub const OPT_IGNORE_ENV: u32 = 1;
pub const OPT_LOCK_CPATH: u32 = 2;
pub const OPT_FFI: u32 = 4;
pub const OPT_GC_STRESS: u32 = 8;
pub const OPT_GENERATIONAL: u32 = 16;

/// A compiled chunk and, when asked for, its listing.
#[stabby::stabby]
#[derive(Debug, Clone)]
pub struct ACompiled {
    pub chunk: Vec<u8>,
    pub listing: String,
    pub listed: bool,
}

/// What a native sees.
#[stabby::stabby(checked)]
pub trait NativeCtx {
    extern "C" fn nargs(&self) -> usize;
    extern "C" fn arg(&mut self, i: usize) -> AValue;
    extern "C" fn push<'a>(&mut self, v: &'a AValue);
    extern "C" fn call<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<Vec<AValue>>;
}

pub type CtxMut<'a> = dynptr!(&'a mut dyn NativeCtx);

/// A program's native function, held by the library.
#[stabby::stabby(checked)]
pub trait HostFn {
    extern "C" fn run<'a>(&self, ctx: CtxMut<'a>) -> Result<usize, String>;
}

pub type HostFnBox = dynptr!(Box<dyn HostFn>);

/// Raises `interrupted!` in a state; usable from any thread.
#[stabby::stabby(checked)]
pub trait Interrupt {
    extern "C" fn request(&self);
    extern "C" fn pending(&self) -> bool;
}

pub type InterruptBox = dynptr!(Box<dyn Interrupt + Send + Sync>);

/// A Lua state. Dropping it closes the state.
#[stabby::stabby(checked)]
pub trait Engine {
    extern "C" fn release(&mut self, id: u32);
    extern "C" fn load<'a>(
        &mut self,
        chunk: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> AResult<AValue>;
    /// `stdin` reads standard input; otherwise `path` is the file.
    extern "C" fn load_file<'a>(&mut self, path: Str<'a>, stdin: bool) -> AResult<AValue>;
    extern "C" fn call<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<Vec<AValue>>;
    extern "C" fn call_with_traceback<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<Vec<AValue>>;
    extern "C" fn global<'a>(&mut self, name: Str<'a>) -> AValue;
    extern "C" fn set_global<'a>(&mut self, name: Str<'a>, v: &'a AValue);
    extern "C" fn new_table(&mut self) -> AValue;
    extern "C" fn get<'a>(&mut self, t: &'a AValue, k: &'a AValue) -> AValue;
    extern "C" fn set<'a>(&mut self, t: &'a AValue, k: &'a AValue, v: &'a AValue) -> AResult<bool>;
    extern "C" fn len<'a>(&mut self, v: &'a AValue) -> i64;
    extern "C" fn tostring<'a>(&mut self, v: &'a AValue) -> AResult<Vec<u8>>;
    extern "C" fn function<'a>(&mut self, name: Str<'a>, f: HostFnBox) -> AValue;
    extern "C" fn set_warnings(&mut self, on: bool);
    extern "C" fn interrupt(&mut self) -> InterruptBox;
    extern "C" fn flush(&mut self);
}

pub type EngineBox = dynptr!(Box<dyn Engine>);

/// The library: states, and the compiler's tools.
#[stabby::stabby(checked)]
pub trait Library {
    extern "C" fn new_state(&self, options: AOptions) -> AResult<EngineBox>;
    /// `stdin` reads standard input; otherwise `path` is the file.
    extern "C" fn read_chunk_file<'a>(
        &self,
        path: Slice<'a, u8>,
        stdin: bool,
    ) -> Result<Tuple2<Vec<u8>, Vec<u8>>, String>;
    extern "C" fn chunk_id<'a>(&self, source: Slice<'a, u8>) -> String;
    extern "C" fn syntax_tree<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> Result<Vec<u8>, String>;
    extern "C" fn hir<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> Result<String, String>;
    extern "C" fn compile<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
        strip: bool,
        list: bool,
    ) -> Result<ACompiled, String>;
}

pub type LibraryBox = dynptr!(Box<dyn Library>);
