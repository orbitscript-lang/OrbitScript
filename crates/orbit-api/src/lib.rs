//! The OrbitScript embedding API that works across a shared-library
//! boundary (ADR-10).
//!
//! A program written against this crate runs the same way whichever
//! backend it is built with:
//!
//! - `static` (the default) links the runtime into the program;
//! - `shared` reaches the runtime in `liborbit` through `stabby`, which
//!   checks the boundary's type layouts when the program first calls in,
//!   and this crate refuses a library built from a different version of
//!   the boundary.
//!
//! The `orbit` crate stays the full Rust API, linked statically; this is
//! the part of it that can cross a library boundary. Values cross by copy
//! ([`Value::Str`] and the scalars) or as [`Ref`]s: handles to a table,
//! function, userdata or thread that keep it alive while any clone of the
//! handle exists.
//!
//! ```no_run
//! use orbit_api::{Options, State, Value};
//!
//! let mut lua = State::new(&Options::default()).unwrap();
//! let f = lua.load(b"return ... + 1", b"=example").unwrap();
//! assert_eq!(lua.call(&f, &[Value::Int(41)]).unwrap(), [Value::Int(42)]);
//! ```

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

#[doc(hidden)]
pub mod raw;
use raw::{Kind, RawCtx, RawVal};

#[cfg(all(feature = "static", not(feature = "shared")))]
#[path = "static_.rs"]
mod imp;
#[cfg(feature = "shared")]
#[path = "shared.rs"]
mod imp;
#[cfg(not(any(feature = "static", feature = "shared")))]
compile_error!("orbit-api needs a backend: the `static` or `shared` feature");

/// The runtime behind the `static` backend, which `liborbit` also wraps.
/// Not part of the API.
#[cfg(feature = "static")]
#[doc(hidden)]
pub mod engine;

pub mod tools;

/// OrbitScript's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A Lua value.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A string, copied.
    Str(Vec<u8>),
    Table(Ref),
    /// A Lua or native function.
    Function(Ref),
    /// Full or light userdata.
    Userdata(Ref),
    Thread(Ref),
}

impl Value {
    /// The name `type` gives this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
            Value::Userdata(_) => "userdata",
            Value::Thread(_) => "thread",
        }
    }

    /// The bytes of a string, or `None`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.as_bytes().to_vec())
    }
}

impl From<&[u8]> for Value {
    fn from(s: &[u8]) -> Value {
        Value::Str(s.to_vec())
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

/// A handle to a table, function, userdata or thread in a [`State`]. The
/// object stays alive while any clone of the handle does. A handle only
/// means something to the state that gave it.
#[derive(Clone)]
pub struct Ref(Rc<RefInner>);

struct RefInner {
    id: u32,
    /// Where the handle goes when dropped, for the state to forget at its
    /// next call.
    drops: Drops,
}

impl Drop for RefInner {
    fn drop(&mut self) {
        self.drops.borrow_mut().push(self.id);
    }
}

impl Ref {
    fn id(&self) -> u32 {
        self.0.id
    }
}

impl PartialEq for Ref {
    /// Whether the two handles came from the same hand-out (not whether
    /// they name the same object: each hand-out is its own handle).
    fn eq(&self, other: &Ref) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ref({})", self.0.id)
    }
}

/// Handles dropped since the state last ran.
type Drops = Rc<RefCell<Vec<u32>>>;

/// Values crossing into the runtime, borrowing the program's.
fn to_raw(v: &Value) -> RawVal {
    match v {
        Value::Nil => RawVal::Nil,
        Value::Bool(b) => RawVal::Bool(*b),
        Value::Int(i) => RawVal::Int(*i),
        Value::Float(f) => RawVal::Float(*f),
        Value::Str(s) => RawVal::Str(s.clone()),
        Value::Table(r) => RawVal::Ref(r.id(), Kind::Table),
        Value::Function(r) => RawVal::Ref(r.id(), Kind::Function),
        Value::Userdata(r) => RawVal::Ref(r.id(), Kind::Userdata),
        Value::Thread(r) => RawVal::Ref(r.id(), Kind::Thread),
    }
}

fn to_raws(vs: &[Value]) -> Vec<RawVal> {
    vs.iter().map(to_raw).collect()
}

/// Values coming back: each reference is a new handle.
fn from_raw(drops: &Drops, v: RawVal) -> Value {
    let r = |id| {
        Ref(Rc::new(RefInner {
            id,
            drops: drops.clone(),
        }))
    };
    match v {
        RawVal::Nil => Value::Nil,
        RawVal::Bool(b) => Value::Bool(b),
        RawVal::Int(i) => Value::Int(i),
        RawVal::Float(f) => Value::Float(f),
        RawVal::Str(s) => Value::Str(s),
        RawVal::Ref(id, Kind::Table) => Value::Table(r(id)),
        RawVal::Ref(id, Kind::Function) => Value::Function(r(id)),
        RawVal::Ref(id, Kind::Userdata) => Value::Userdata(r(id)),
        RawVal::Ref(id, Kind::Thread) => Value::Thread(r(id)),
    }
}

fn from_raws(drops: &Drops, vs: Vec<RawVal>) -> Vec<Value> {
    vs.into_iter().map(|v| from_raw(drops, v)).collect()
}

/// An error from Lua.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A chunk failed to compile, or a file failed to load.
    Syntax(String),
    /// Code raised an error; `message` is what the standalone interpreter
    /// would print for its value.
    Runtime {
        message: String,
        traceback: Option<String>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Syntax(m) | Error::Runtime { message: m, .. } => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

/// How to set a state up.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Ignore `LUA_PATH`, `LUA_CPATH` and the like, as `lua -E` does.
    pub ignore_env: bool,
    /// Load C modules only from the starting `package.cpath` (ADR-8).
    pub lock_cpath: bool,
    /// Open the `ffi` library, when the runtime has it (ADR-9).
    pub ffi: bool,
    /// Cap the Lua heap at this many bytes: past it, scripts get the error
    /// `not enough memory`.
    pub memory_limit: Option<usize>,
    /// Collect at nearly every safe point after an allocation (testing).
    pub gc_stress: bool,
    /// Report the collector as generational, as the standalone `lua` does
    /// from the start (the incremental collector runs either way).
    pub generational: bool,
}

/// Raises `interrupted!` in a state's running Lua code. It can be sent to
/// another thread, such as one waiting for Ctrl-C.
pub struct Interrupt(imp::Interrupt);

impl Interrupt {
    /// Ask the state to raise `interrupted!` at its next check. Only sets
    /// a flag.
    pub fn request(&self) {
        self.0.request()
    }

    /// Whether a request is waiting to be raised.
    pub fn pending(&self) -> bool {
        self.0.pending()
    }
}

impl fmt::Debug for Interrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interrupt").finish_non_exhaustive()
    }
}

/// What a native function sees.
pub trait Ctx {
    /// How many arguments it got.
    fn nargs(&self) -> usize;
    /// Argument `i` (from 0), or nil.
    fn arg(&mut self, i: usize) -> Value;
    /// Push a result.
    fn push(&mut self, v: &Value);
    /// Call `f` with `args` and return its results.
    fn call(&mut self, f: &Value, args: &[Value]) -> Result<Vec<Value>, Error>;
}

/// A native function: returns how many of the values it pushed are its
/// results, or an error message.
pub type NativeFn = dyn Fn(&mut dyn Ctx) -> Result<usize, String>;

/// [`Ctx`] over the backend's, handing out handles that report to `drops`.
struct NativeCtx<'a> {
    raw: &'a mut dyn RawCtx,
    drops: &'a Drops,
}

impl Ctx for NativeCtx<'_> {
    fn nargs(&self) -> usize {
        self.raw.nargs()
    }
    fn arg(&mut self, i: usize) -> Value {
        from_raw(self.drops, self.raw.arg(i))
    }
    fn push(&mut self, v: &Value) {
        self.raw.push(&to_raw(v))
    }
    fn call(&mut self, f: &Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        let out = self.raw.call(&to_raw(f), &to_raws(args))?;
        Ok(from_raws(self.drops, out))
    }
}

/// A Lua state with the standard libraries open. Dropping it closes it, as
/// `lua_close` does: every pending finalizer runs.
pub struct State {
    imp: imp::State,
    drops: Drops,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

impl State {
    /// A new state.
    pub fn new(options: &Options) -> Result<State, Error> {
        Ok(State {
            imp: imp::State::new(options)?,
            drops: Drops::default(),
        })
    }

    /// Let the runtime forget the handles the program dropped.
    fn sweep(&mut self) {
        let dropped = std::mem::take(&mut *self.drops.borrow_mut());
        for id in dropped {
            self.imp.release(id);
        }
    }

    fn val(&self, v: RawVal) -> Value {
        from_raw(&self.drops, v)
    }

    /// Compile a chunk, Lua or OrbitScript (by `source`'s `.orb` suffix),
    /// source or binary. `source` names it as `load` takes it: `@file`,
    /// `=name`, or the code itself.
    pub fn load(&mut self, chunk: &[u8], source: &[u8]) -> Result<Value, Error> {
        self.sweep();
        let f = self.imp.load(chunk, source)?;
        Ok(self.val(f))
    }

    /// Load a file as `loadfile` does (skipping a first `#` line); `None`
    /// reads standard input, named `=stdin`.
    pub fn load_file(&mut self, path: Option<&str>) -> Result<Value, Error> {
        self.sweep();
        let f = self.imp.load_file(path)?;
        Ok(self.val(f))
    }

    /// Call `f` with `args`, returning its results.
    pub fn call(&mut self, f: &Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        self.sweep();
        let out = self.imp.call(&to_raw(f), &to_raws(args))?;
        Ok(from_raws(&self.drops, out))
    }

    /// Call `f` as the standalone `lua` does: an error's message is the
    /// text it prints, the error value (or its `__tostring`) plus a
    /// traceback made while the failing code's stack was still there.
    pub fn call_with_traceback(&mut self, f: &Value, args: &[Value]) -> Result<Vec<Value>, Error> {
        self.sweep();
        let out = self.imp.call_with_traceback(&to_raw(f), &to_raws(args))?;
        Ok(from_raws(&self.drops, out))
    }

    /// Read a global.
    pub fn global(&mut self, name: &str) -> Value {
        self.sweep();
        let v = self.imp.global(name);
        self.val(v)
    }

    /// Set a global.
    pub fn set_global(&mut self, name: &str, v: &Value) {
        self.sweep();
        self.imp.set_global(name, &to_raw(v))
    }

    /// A new empty table.
    pub fn new_table(&mut self) -> Value {
        self.sweep();
        let t = self.imp.new_table();
        self.val(t)
    }

    /// `t[k]`, raw. Nil when `t` is not a table.
    pub fn get(&mut self, t: &Value, k: &Value) -> Value {
        self.sweep();
        let v = self.imp.get(&to_raw(t), &to_raw(k));
        self.val(v)
    }

    /// `t[k] = v`, raw. Fails when `t` is not a table or `k` is nil or NaN.
    pub fn set(&mut self, t: &Value, k: &Value, v: &Value) -> Result<(), Error> {
        self.sweep();
        self.imp.set(&to_raw(t), &to_raw(k), &to_raw(v))
    }

    /// `#v` without metamethods: a string's bytes or a table's border; 0
    /// for anything else.
    pub fn len(&mut self, v: &Value) -> i64 {
        self.sweep();
        self.imp.len(&to_raw(v))
    }

    /// `v` as `tostring` shows it, through `__tostring` when it has one.
    pub fn to_string(&mut self, v: &Value) -> Result<Vec<u8>, Error> {
        self.sweep();
        self.imp.tostring(&to_raw(v))
    }

    /// A native function, named `name` in messages.
    pub fn function(
        &mut self,
        name: &str,
        f: impl Fn(&mut dyn Ctx) -> Result<usize, String> + 'static,
    ) -> Value {
        self.sweep();
        let drops = self.drops.clone();
        let raw: raw::RawNative =
            Box::new(move |raw: &mut dyn RawCtx| f(&mut NativeCtx { raw, drops: &drops }));
        let v = self.imp.function(name, raw);
        self.val(v)
    }

    /// Switch warnings (`warn`) on or off, as `-W` and `@on` do.
    pub fn set_warnings(&mut self, on: bool) {
        self.imp.set_warnings(on)
    }

    /// A handle that interrupts this state's running code.
    pub fn interrupt(&mut self) -> Interrupt {
        Interrupt(self.imp.interrupt())
    }

    /// Flush `print` output and open files.
    pub fn flush(&mut self) {
        self.imp.flush()
    }
}
