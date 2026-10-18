//! Thread values: coroutines ([VM semantics]).
//!
//! The VM owns what a coroutine is (its stack and status); this crate only
//! knows that a thread is a heap object whose state the collector traces.
//!
//! [VM semantics]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use std::any::Any;
use std::fmt;

use orbit_gc::{Marker, Trace};

use crate::object::LuaSpace;

/// What the VM keeps in a [`Thread`].
pub trait ThreadState: Any + fmt::Debug {
    /// Mark every object the state refers to.
    fn trace(&self, m: &mut Marker<'_, LuaSpace>);
    /// Heap bytes the state owns, for the collector's accounting. When
    /// this changes, the owner reports the difference
    /// ([`Heap::adjust_bytes`](orbit_gc::Heap::adjust_bytes)).
    fn extra_bytes(&self) -> usize;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// A thread (coroutine) value.
#[derive(Debug)]
pub struct Thread {
    pub state: Box<dyn ThreadState>,
}

impl Thread {
    pub fn new(state: impl ThreadState) -> Self {
        Thread {
            state: Box::new(state),
        }
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
