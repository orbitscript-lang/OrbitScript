//! Typed arenas and the handles that point into them.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

const PAGE_BITS: u32 = 10;
const PAGE: usize = 1 << PAGE_BITS;

/// Storage for every object of one type.
///
/// Slots live in fixed pages of 1024. Pages never move, so growing an arena
/// never copies live objects and never doubles peak memory, unlike a single
/// `Vec`. Colors, the free list and the slot count are kept by the collector,
/// not here, so an object can be read while the collector writes colors.
///
/// Arenas are created by [`space!`](crate::space) and only reached through a
/// [`Heap`](crate::Heap).
pub struct Arena<T> {
    pages: Vec<Box<[Option<T>]>>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena { pages: Vec::new() }
    }
}

impl<T> fmt::Debug for Arena<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arena")
            .field("pages", &self.pages.len())
            .finish()
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn stale(i: u32) -> ! {
    panic!("orbit-gc: stale handle (slot {i} is free)")
}

impl<T> Arena<T> {
    #[inline(always)]
    fn slot(&self, i: u32) -> &Option<T> {
        &self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)]
    }

    #[inline(always)]
    fn slot_mut(&mut self, i: u32) -> &mut Option<T> {
        &mut self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)]
    }

    /// The object in slot `i`. Panics if the slot is free.
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get(&self, i: u32) -> &T {
        match self.slot(i) {
            Some(v) => v,
            None => stale(i),
        }
    }

    #[inline(always)]
    #[track_caller]
    pub(crate) fn get_mut(&mut self, i: u32) -> &mut T {
        match self.slot_mut(i) {
            Some(v) => v,
            None => stale(i),
        }
    }

    pub(crate) fn is_occupied(&self, i: u32) -> bool {
        (i as usize) < self.pages.len() * PAGE && self.slot(i).is_some()
    }

    /// Store `v` in slot `i`, adding a page when `i` is the first slot past
    /// the end. The collector hands out indexes densely, so that is the only
    /// way `i` can be out of range.
    pub(crate) fn put(&mut self, i: u32, v: T) {
        if i as usize >= self.pages.len() * PAGE {
            debug_assert_eq!(i as usize, self.pages.len() * PAGE);
            self.pages.push((0..PAGE).map(|_| None).collect());
        }
        *self.slot_mut(i) = Some(v);
    }

    pub(crate) fn take(&mut self, i: u32) -> T {
        match self.slot_mut(i).take() {
            Some(v) => v,
            None => stale(i),
        }
    }
}

/// A handle to a `T` in a [`Heap`](crate::Heap).
///
/// A `Gc` is a 4-byte slot index: `Copy`, with no lifetime and no reference
/// count. It stays valid only while the object is reachable from the roots
/// the collector is given (see [`Roots`](crate::Roots)). Using a handle after
/// its object was collected is a logic error, never undefined behavior: it
/// panics if the slot is still free, or reads whichever object reused it.
pub struct Gc<T> {
    index: u32,
    _t: PhantomData<fn() -> T>,
}

impl<T> Gc<T> {
    /// The slot index, for packing a handle into a tagged value.
    #[inline(always)]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Rebuild a handle from [`Gc::index`]. A wrong index gives a handle to
    /// some other object of the same type, or one that panics on use.
    #[inline(always)]
    pub const fn from_index(index: u32) -> Self {
        Gc {
            index,
            _t: PhantomData,
        }
    }
}

impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Gc<T> {}
impl<T> PartialEq for Gc<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}
impl<T> Eq for Gc<T> {}
impl<T> PartialOrd for Gc<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Gc<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.index.cmp(&other.index)
    }
}
impl<T> Hash for Gc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state)
    }
}
impl<T> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = std::any::type_name::<T>();
        let short = name.rsplit("::").next().unwrap_or(name);
        write!(f, "Gc<{short}>#{}", self.index)
    }
}

/// A handle whose type is only known at run time: a kind number (the order
/// of the arena in its [`space!`](crate::space)) plus a slot index.
///
/// Finalization queues hand these out, since objects of several types can be
/// finalizable. Turn one back into a typed handle with
/// [`Heap::downcast`](crate::Heap::downcast).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AnyGc {
    pub(crate) kind: u8,
    pub(crate) index: u32,
}

impl AnyGc {
    /// The arena this object lives in.
    pub fn kind(self) -> u8 {
        self.kind
    }

    /// The slot index within its arena.
    pub fn index(self) -> u32 {
        self.index
    }
}
