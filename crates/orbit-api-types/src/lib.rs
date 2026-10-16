//! SPIKE: what a program using the embedding API sees, whichever way the
//! runtime is linked.

/// A value. Scalars and strings are copies; tables, functions, userdata and
/// threads are numbered references the runtime keeps alive until released.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Ref(u32),
}

/// What a native function sees.
pub trait Ctx {
    /// How many arguments it got.
    fn nargs(&self) -> usize;
    /// Argument `i` (from 0), or nil.
    fn arg(&mut self, i: usize) -> Val;
    /// Push a result.
    fn push(&mut self, v: &Val);
    /// Call `f` with `args` and return its results.
    fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String>;
}

/// A native function: returns how many of the values it pushed are its
/// results, or an error message.
pub type NativeFn = fn(&mut dyn Ctx) -> Result<usize, String>;
