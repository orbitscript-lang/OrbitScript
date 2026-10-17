//! What the backends trade: values with references as plain numbers, which
//! [`crate::State`] turns into [`crate::Ref`] handles. Not part of the API;
//! public for `liborbit`, which carries these across the library boundary.

use crate::Error;

/// What a reference names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Table,
    Function,
    Userdata,
    Thread,
}

/// A value, with references numbered.
#[derive(Clone, Debug, PartialEq)]
pub enum RawVal {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Ref(u32, Kind),
}

/// What a native sees, in [`RawVal`]s.
pub trait RawCtx {
    fn nargs(&self) -> usize;
    fn arg(&mut self, i: usize) -> RawVal;
    fn push(&mut self, v: &RawVal);
    fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error>;
}

/// A native over [`RawCtx`].
pub type RawNative = Box<dyn Fn(&mut dyn RawCtx) -> Result<usize, String>>;
