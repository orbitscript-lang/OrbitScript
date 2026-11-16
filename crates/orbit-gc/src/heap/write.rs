//! Mutable access to objects that can hold handles: [`Write`], the guard
//! that the barrier calls return.
//!
//! An object the collector has already traced must not be made to point at
//! one it hasn't unless a write barrier tells it. So [`Heap::get_mut`] is
//! only for leaf objects, which hold no handles, and every other object is
//! changed through a guard that the barrier itself hands out:
//!
//! | Call | Barrier | For |
//! |---|---|---|
//! | [`Heap::write_back`] | backward | storing handles into an object written often |
//! | [`Heap::write_at`] | backward, for one element | indexed stores into big objects |
//! | [`Heap::write_for`] | backward, if the value can be a handle | the interpreter's stores, where numbers must not pay |
//! | [`Heap::write_at_for`] | as `write_at`, if the value can be a handle | the interpreter's indexed stores |
//! | [`Heap::write_no_handles`] | none | changes that store no handle: a status, a counter |
//! | [`Heap::update`], [`Heap::update_by`] | backward, plus byte accounting | changes that can change the object's size |
//!
//! The guard borrows the heap, so no collector step can come between the
//! barrier and the store. In debug builds it checks itself when dropped:
//! the object's [`Trace::extra_bytes`] must not have changed (that takes
//! [`Heap::update`]), and after [`Heap::write_no_handles`] the object must
//! hold no handle it didn't hold before. A failed check panics where the
//! guard was made.
//!
//! What the guards cannot see: a store through interior mutability (a
//! `Cell` in a traced object) bypasses them, and the handles an object is
//! given when it is allocated need no barrier.

use super::Heap;
use crate::arena::{Gc, Store};
use crate::trace::{Holds, Space, Trace};
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

#[cfg(debug_assertions)]
use crate::arena::AnyGc;

/// Whether a value stored into an object may be a handle, for
/// [`Heap::write_for`]: storing one that can't be (a number, say) needs no
/// barrier. Implement it `#[inline(always)]`, so that when the caller knows
/// the value's variant the test folds away.
///
/// [`Heap::write_for`] takes the value by reference and
/// [`Heap::write_at_for`] by value. Either works for the barrier; these are
/// the shapes with which the interpreter loop that inlines both compiles
/// best on x86-64 and arm64 together (passing the table store's value by
/// reference made the loop spill registers on paths that store nothing).
pub trait HoldsHandle {
    /// False only if the value is certainly not a handle.
    fn holds_handle(&self) -> bool;
}

impl<T> HoldsHandle for Gc<T> {
    #[inline(always)]
    fn holds_handle(&self) -> bool {
        true
    }
}

impl<T: HoldsHandle> HoldsHandle for Option<T> {
    #[inline(always)]
    fn holds_handle(&self) -> bool {
        self.as_ref().is_some_and(T::holds_handle)
    }
}

/// Mutable access to an object after its write barrier, from
/// [`Heap::write_back`] and its siblings (see the [module](self) docs).
/// It behaves as `&mut T` and borrows the heap until it is dropped.
#[must_use = "the guard is the access: write through it"]
pub struct Write<'h, S: Space + Holds<T>, T: Trace<S>> {
    #[cfg(not(debug_assertions))]
    obj: &'h mut T,
    #[cfg(debug_assertions)]
    check: Check<'h, S, T>,
    _s: PhantomData<fn() -> S>,
}

/// What a guard checks when dropped, in debug builds. The heap is `None`
/// once the guard has given up its checks ([`Write::into_untraced`]).
#[cfg(debug_assertions)]
struct Check<'h, S: Space + Holds<T>, T: Trace<S>> {
    heap: Option<&'h mut Heap<S>>,
    gc: Gc<T>,
    /// The object's extra bytes when the guard was made.
    bytes: usize,
    /// For [`Heap::write_no_handles`]: the handles the object held, sorted.
    handles: Option<Vec<AnyGc>>,
    /// Where the guard was made, and by which call, for the report.
    at: &'static std::panic::Location<'static>,
    what: &'static str,
}

impl<'h, S: Space + Holds<T>, T: Trace<S>> Write<'h, S, T> {
    #[inline(always)]
    #[track_caller]
    pub(super) fn new(
        heap: &'h mut Heap<S>,
        gc: Gc<T>,
        what: &'static str,
        no_handles: bool,
    ) -> Self {
        #[cfg(not(debug_assertions))]
        {
            let _ = (what, no_handles);
            Write {
                obj: heap.space.arena_mut().get_mut(gc),
                _s: PhantomData,
            }
        }
        #[cfg(debug_assertions)]
        {
            let bytes = heap.space.arena().get(gc).extra_bytes();
            let handles = no_handles.then(|| heap.handles_of(gc));
            Write {
                check: Check {
                    heap: Some(heap),
                    gc,
                    bytes,
                    handles,
                    at: std::panic::Location::caller(),
                    what,
                },
                _s: PhantomData,
            }
        }
    }

    /// End the guard but keep the access for as long as the heap stays
    /// borrowed, without the debug checks. Only for handing out a part of
    /// the object the collector never looks at, such as a userdata's host
    /// value.
    #[inline(always)]
    pub fn into_untraced(self) -> &'h mut T {
        #[cfg(not(debug_assertions))]
        {
            self.obj
        }
        #[cfg(debug_assertions)]
        {
            let mut me = self;
            let gc = me.check.gc;
            let heap = me.check.heap.take().expect("a guard has its heap");
            heap.space.arena_mut().get_mut(gc)
        }
    }
}

impl<S: Space + Holds<T>, T: Trace<S>> Deref for Write<'_, S, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        #[cfg(not(debug_assertions))]
        {
            self.obj
        }
        #[cfg(debug_assertions)]
        {
            let heap = self.check.heap.as_ref().expect("a guard has its heap");
            heap.space.arena().get(self.check.gc)
        }
    }
}

impl<S: Space + Holds<T>, T: Trace<S>> DerefMut for Write<'_, S, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        #[cfg(not(debug_assertions))]
        {
            self.obj
        }
        #[cfg(debug_assertions)]
        {
            let gc = self.check.gc;
            let heap = self.check.heap.as_mut().expect("a guard has its heap");
            heap.space.arena_mut().get_mut(gc)
        }
    }
}

impl<S: Space + Holds<T>, T: Trace<S> + fmt::Debug> fmt::Debug for Write<'_, S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Write").field(&**self).finish()
    }
}

#[cfg(debug_assertions)]
impl<S: Space + Holds<T>, T: Trace<S>> Drop for Write<'_, S, T> {
    fn drop(&mut self) {
        let c = &mut self.check;
        let Some(heap) = c.heap.take() else {
            return;
        };
        if std::thread::panicking() {
            return;
        }
        let name = S::kind_name(<S as Holds<T>>::KIND);
        let index = c.gc.index();
        let bytes = heap.space.arena().get(c.gc).extra_bytes();
        assert!(
            bytes == c.bytes,
            "orbit-gc write guard: {name}#{index}'s extra bytes went from {} to {bytes} \
             through the {} at {}: a change of size needs Heap::update",
            c.bytes,
            c.what,
            c.at,
        );
        if let Some(before) = &c.handles {
            let after = heap.handles_of(c.gc);
            if let Some(new) = after.iter().find(|h| before.binary_search(h).is_err()) {
                panic!(
                    "orbit-gc write guard: {name}#{index} was given a handle to {}#{} through \
                     the write_no_handles at {}: a store of a handle needs a write barrier \
                     (write_back, write_at or write_for)",
                    S::kind_name(new.kind),
                    new.index,
                    c.at,
                );
            }
        }
    }
}

impl<S: Space> Heap<S> {
    /// The backward barrier ([`Heap::barrier_back`]), then mutable access:
    /// for storing handles into an object written often, such as a table.
    #[inline(always)]
    #[track_caller]
    pub fn write_back<T: Trace<S>>(&mut self, gc: Gc<T>) -> Write<'_, S, T>
    where
        S: Holds<T>,
    {
        self.barrier_back(gc);
        Write::new(self, gc, "write_back", false)
    }

    /// The barrier for element `at` of the object's indexed part
    /// ([`Heap::barrier_at`]), then mutable access: for an indexed store
    /// into a big object.
    #[inline(always)]
    #[track_caller]
    pub fn write_at<T: Trace<S>>(&mut self, gc: Gc<T>, at: usize) -> Write<'_, S, T>
    where
        S: Holds<T>,
    {
        self.barrier_at(gc, at);
        Write::new(self, gc, "write_at", false)
    }

    /// [`Heap::write_back`] for storing `value`, with the barrier only if
    /// `value` may be a handle ([`HoldsHandle`]): numbers stored into an
    /// old object don't remember it.
    #[inline(always)]
    #[track_caller]
    pub fn write_for<T: Trace<S>, V: HoldsHandle + ?Sized>(
        &mut self,
        gc: Gc<T>,
        value: &V,
    ) -> Write<'_, S, T>
    where
        S: Holds<T>,
    {
        if value.holds_handle() {
            self.barrier_back(gc);
        }
        Write::new(self, gc, "write_for", false)
    }

    /// [`Heap::write_at`] for storing `value`, with the barrier only if
    /// `value` may be a handle.
    #[inline(always)]
    #[track_caller]
    pub fn write_at_for<T: Trace<S>, V: HoldsHandle>(
        &mut self,
        gc: Gc<T>,
        at: usize,
        value: V,
    ) -> Write<'_, S, T>
    where
        S: Holds<T>,
    {
        if value.holds_handle() {
            self.barrier_at(gc, at);
        }
        Write::new(self, gc, "write_at_for", false)
    }

    /// Mutable access with no barrier, for a change that stores no handle
    /// into the object: a status, a counter, code swapped for code with the
    /// same constants. Handles may be moved within the object or dropped
    /// from it. Debug builds check, when the guard is dropped, that the
    /// object holds no handle it didn't hold before.
    #[inline(always)]
    #[track_caller]
    pub fn write_no_handles<T: Trace<S>>(&mut self, gc: Gc<T>) -> Write<'_, S, T>
    where
        S: Holds<T>,
    {
        Write::new(self, gc, "write_no_handles", true)
    }

    /// The handles object `gc` holds, sorted, as its `trace` reports them.
    #[cfg(debug_assertions)]
    fn handles_of<T: Trace<S>>(&mut self, gc: Gc<T>) -> Vec<AnyGc>
    where
        S: Holds<T>,
    {
        let kind = <S as Holds<T>>::KIND;
        let check = &mut self.marks.check;
        check.collecting = true;
        check.active = true;
        check.collected.clear();
        let mut m = crate::trace::Marker {
            space: &self.space,
            seed: self.seed,
            marks: &mut self.marks,
            weak: None,
            can_register: false,
        };
        self.space.trace_slot(kind, gc.index(), &mut m);
        let check = &mut self.marks.check;
        check.collecting = false;
        check.active = false;
        let mut handles = std::mem::take(&mut check.collected);
        handles.sort_unstable();
        handles.dedup();
        handles
    }
}
