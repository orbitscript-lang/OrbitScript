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
}

/// A thread (coroutine) value.
///
/// The state is kept as `dyn Any`, so the VM reaches it with one type check
/// on every switch, with [`ThreadState`]'s methods beside it for the
/// collector. (A `Box<dyn ThreadState>` would need trait upcasting, Rust
/// 1.86, to downcast without an extra indirect call.)
pub struct Thread {
    pub state: Box<dyn Any>,
    trace: fn(&dyn Any, &mut Marker<'_, LuaSpace>),
    extra_bytes: fn(&dyn Any) -> usize,
    debug: fn(&dyn Any, &mut fmt::Formatter<'_>) -> fmt::Result,
}

impl Thread {
    pub fn new<S: ThreadState>(state: S) -> Self {
        fn get<S: ThreadState>(a: &dyn Any) -> &S {
            a.downcast_ref().expect("a thread's state keeps its type")
        }
        Thread {
            state: Box::new(state),
            trace: |a, m| get::<S>(a).trace(m),
            extra_bytes: |a| get::<S>(a).extra_bytes(),
            debug: |a, f| fmt::Debug::fmt(get::<S>(a), f),
        }
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Thread")
            .field("state", &DebugState(self))
            .finish()
    }
}

struct DebugState<'a>(&'a Thread);

impl fmt::Debug for DebugState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (self.0.debug)(&*self.0.state, f)
    }
}

impl Trace<LuaSpace> for Thread {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        (self.trace)(&*self.state, m);
    }

    fn extra_bytes(&self) -> usize {
        (self.extra_bytes)(&*self.state)
    }
}
