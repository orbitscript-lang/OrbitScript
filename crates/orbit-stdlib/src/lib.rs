//! The Lua 5.5 standard library (stream D).
//!
//! Two layers:
//!
//! - Pure functions over byte slices and plain values, ported from PUC Lua 5.5 so
//!   results and error messages match the reference interpreter: [`pattern`] (Lua
//!   patterns), [`format`] (`string.format`), [`pack`] (`string.pack` and friends) and
//!   [`random`] (the `xoshiro256**` generator of `math.random`). They report
//!   [`LibError`]s and reach Lua values through small traits.
//! - The libraries themselves, as native functions on `orbit-vm`: `string`, `table`,
//!   `math`, `utf8`, `os`, `io`, `package` and part of `debug`. [`open_libs`] opens
//!   them all.
//!
//! The basic and coroutine libraries live in `orbit-vm` (`baselib.rs`), which opens them.

mod debuglib;
mod error;
pub mod format;
mod iolib;
mod mathlib;
mod numfmt;
mod oslib;
pub mod pack;
pub mod pattern;
mod pkglib;
pub mod random;
mod strlib;
mod tablib;
mod utf8lib;

pub use debuglib::open_debug;
pub use error::LibError;
pub use iolib::open_io;
pub use mathlib::open_math;
pub use oslib::open_os;
pub use pkglib::open_package;
pub use strlib::open_string;
pub use tablib::open_table;
pub use utf8lib::open_utf8;

use orbit_vm::{Error, Vm};

/// Opens every library in this crate (the VM opens the basic and coroutine libraries):
/// `package`, `string`, `table`, `math`, `utf8`, `os`, `io` and `debug`.
pub fn open_libs(vm: &mut Vm) {
    open_package(vm);
    open_string(vm);
    open_table(vm);
    open_math(vm);
    open_utf8(vm);
    open_os(vm);
    open_io(vm);
    open_debug(vm);
}

/// Turns a [`LibError`] into a Lua error raised by the running native function.
pub(crate) fn raise(vm: &mut Vm, e: LibError) -> Error {
    match e {
        LibError::Message(m) => vm.error(m),
        LibError::Arg { arg, msg } => vm.arg_error(arg, &String::from_utf8_lossy(&msg)),
    }
}

/// Either kind of error, for the traits the pure layer uses (`FormatArgs`, ...).
pub(crate) enum AnyError {
    Lib(LibError),
    Vm(Error),
}

impl From<LibError> for AnyError {
    fn from(e: LibError) -> Self {
        AnyError::Lib(e)
    }
}

impl From<Error> for AnyError {
    fn from(e: Error) -> Self {
        AnyError::Vm(e)
    }
}

impl AnyError {
    pub(crate) fn into_error(self, vm: &mut Vm) -> Error {
        match self {
            AnyError::Lib(e) => raise(vm, e),
            AnyError::Vm(e) => e,
        }
    }
}
