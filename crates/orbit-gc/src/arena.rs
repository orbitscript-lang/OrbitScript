//! Typed arenas and the handles that point into them.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

const PAGE_BITS: u32 = 10;
const PAGE: usize = 1 << PAGE_BITS;

/// Whether handles carry a generation that is checked on every access, so a
/// handle to a collected object panics even after its slot is reused. On in
/// debug builds and with the `checked-handles` feature; off otherwise, where
/// a `Gc` is 4 bytes and access skips the check.
pub const CHECKED_HANDLES: bool = cfg!(any(debug_assertions, feature = "checked-handles"));

/// A slot's generation: bumped each time the slot is freed. Zero-sized when
/// handles are unchecked.
#[cfg(any(debug_assertions, feature = "checked-handles"))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub(crate) struct Gen(u32);

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub(crate) struct Gen;

#[cfg(any(debug_assertions, feature = "checked-handles"))]
impl Gen {
    #[inline(always)]
    pub(crate) const fn get(self) -> u32 {
        self.0
    }
    #[inline(always)]
    pub(crate) const fn new(g: u32) -> Self {
        Gen(g)
    }
}

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
impl Gen {
    #[inline(always)]
    pub(crate) const fn get(self) -> u32 {
        0
    }
    #[inline(always)]
    pub(crate) const fn new(_: u32) -> Self {
        Gen
    }
}

/// The generation of every slot in an arena (nothing when unchecked).
#[cfg(any(debug_assertions, feature = "checked-handles"))]
#[derive(Default)]
struct Gens(Vec<u32>);

#[cfg(any(debug_assertions, feature = "checked-handles"))]
impl Gens {
    #[inline(always)]
    fn of(&self, i: u32) -> Gen {
        Gen(self.0[i as usize])
    }
    fn on_put(&mut self, i: u32) {
        if i as usize == self.0.len() {
            self.0.push(0);
        }
    }
    fn on_take(&mut self, i: u32) {
        let g = &mut self.0[i as usize];
        *g = g.wrapping_add(1);
    }
}

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
#[derive(Default)]
struct Gens;

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
impl Gens {
    #[inline(always)]
    fn of(&self, _: u32) -> Gen {
        Gen
    }
    #[inline(always)]
    fn on_put(&mut self, _: u32) {}
    #[inline(always)]
    fn on_take(&mut self, _: u32) {}
}

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
    gens: Gens,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            pages: Vec::new(),
            gens: Gens::default(),
        }
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
    panic!("orbit-gc: stale handle (slot {i} was freed)")
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

    /// The object `gc` points to. Panics if its slot is free or, with checked
    /// handles, was freed and reused since the handle was made.
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get(&self, gc: Gc<T>) -> &T {
        if self.gens.of(gc.index) != gc.generation {
            stale(gc.index)
        }
        self.get_slot(gc.index)
    }

    #[inline(always)]
    #[track_caller]
    pub(crate) fn get_mut(&mut self, gc: Gc<T>) -> &mut T {
        if self.gens.of(gc.index) != gc.generation {
            stale(gc.index)
        }
        match self.slot_mut(gc.index) {
            Some(v) => v,
            None => stale(gc.index),
        }
    }

    /// The object in slot `i`, whatever its generation. For the collector,
    /// which only visits occupied slots.
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get_slot(&self, i: u32) -> &T {
        match self.slot(i) {
            Some(v) => v,
            None => stale(i),
        }
    }

    #[track_caller]
    pub(crate) fn get_slot_mut(&mut self, i: u32) -> &mut T {
        match self.slot_mut(i) {
            Some(v) => v,
            None => stale(i),
        }
    }

    /// A handle to the object now in slot `i`.
    pub(crate) fn handle_at(&self, i: u32) -> Gc<T> {
        Gc {
            index: i,
            generation: self.gens.of(i),
            _t: PhantomData,
        }
    }

    pub(crate) fn is_occupied(&self, gc: Gc<T>) -> bool {
        (gc.index as usize) < self.pages.len() * PAGE
            && self.gens.of(gc.index) == gc.generation
            && self.slot(gc.index).is_some()
    }

    /// Store `v` in slot `i`, adding a page when `i` is the first slot past
    /// the end. The collector hands out indexes densely, so that is the only
    /// way `i` can be out of range.
    pub(crate) fn put(&mut self, i: u32, v: T) -> Gc<T> {
        if i as usize >= self.pages.len() * PAGE {
            debug_assert_eq!(i as usize, self.pages.len() * PAGE);
            self.pages.push((0..PAGE).map(|_| None).collect());
        }
        self.gens.on_put(i);
        *self.slot_mut(i) = Some(v);
        self.handle_at(i)
    }

    pub(crate) fn take(&mut self, i: u32) -> T {
        self.gens.on_take(i);
        match self.slot_mut(i).take() {
            Some(v) => v,
            None => stale(i),
        }
    }
}

/// A handle to a `T` in a [`Heap`](crate::Heap).
///
/// A `Gc` is a slot index, `Copy`, with no lifetime and no reference count.
/// It stays valid only while the object is reachable from the roots the
/// collector is given (see [`Roots`](crate::Roots)). Using a handle after its
/// object was collected is a logic error, never undefined behavior:
///
/// - With [`CHECKED_HANDLES`] (debug builds, or the `checked-handles`
///   feature), a `Gc` is 8 bytes: the index plus the slot's generation, which
///   every access compares, so a stale handle always panics.
/// - Otherwise a `Gc` is 4 bytes and unchecked. A stale handle panics while
///   its slot is still free, and reads whichever object reused it after that.
pub struct Gc<T> {
    index: u32,
    generation: Gen,
    _t: PhantomData<fn() -> T>,
}

impl<T> Gc<T> {
    /// The slot index, for packing a handle into a tagged value.
    #[inline(always)]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// A handle to slot `index` at generation 0: the first object ever
    /// stored there. With checked handles it panics on use once that slot has
    /// been freed. To rebuild a handle that must stay valid, keep
    /// [`Gc::to_bits`], or use [`Heap::handle_at`](crate::Heap::handle_at).
    #[inline(always)]
    pub const fn from_index(index: u32) -> Self {
        Gc {
            index,
            generation: Gen::new(0),
            _t: PhantomData,
        }
    }

    /// The slot's generation when the handle was made (0 when unchecked).
    #[inline(always)]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// The handle as 8 bytes: the index in the low half, the generation in
    /// the high half. For packing a handle into a tagged value.
    #[inline(always)]
    pub const fn to_bits(self) -> u64 {
        self.index as u64 | (self.generation.get() as u64) << 32
    }

    /// Inverse of [`Gc::to_bits`].
    #[inline(always)]
    pub const fn from_bits(bits: u64) -> Self {
        Gc {
            index: bits as u32,
            generation: Gen::new((bits >> 32) as u32),
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
        self.index == other.index && self.generation == other.generation
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
        self.to_bits().cmp(&other.to_bits())
    }
}
impl<T> Hash for Gc<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.to_bits().hash(state)
    }
}
impl<T> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = std::any::type_name::<T>();
        let short = name.rsplit("::").next().unwrap_or(name);
        if CHECKED_HANDLES {
            write!(f, "Gc<{short}>#{}.{}", self.index, self.generation())
        } else {
            write!(f, "Gc<{short}>#{}", self.index)
        }
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
    pub(crate) generation: Gen,
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
