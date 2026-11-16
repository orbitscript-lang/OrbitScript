//! Thread values: coroutines ([VM semantics]).
//!
//! The VM owns what a coroutine is (its stack and status,
//! [`Coroutine`]); the object layer only holds it.
//!
//! [VM semantics]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use std::fmt;

use orbit_gc::{Marker, Trace};

use crate::object::LuaSpace;
use crate::vm::Coroutine;

/// A thread (coroutine) value: the VM's [`Coroutine`], boxed so a thread's
/// arena slot stays small.
pub struct Thread {
    pub(crate) state: Box<Coroutine>,
}

impl Thread {
    pub(crate) fn new(state: Coroutine) -> Self {
        Thread {
            state: Box::new(state),
        }
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thread")
            .field("state", &self.state)
            .finish()
    }
}

impl Trace<LuaSpace> for Thread {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        self.state.trace(m);
    }

    fn extra_bytes(&self) -> usize {
        self.state.extra_bytes()
    }
}
