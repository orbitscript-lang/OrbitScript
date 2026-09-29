//! Typed arenas and the handles that point into them.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

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

/// The generations of one page's slots, plus the floor a page starts from
/// when it is allocated again after being released, so a handle made before
/// the release can never match a slot of the new page.
#[cfg(any(debug_assertions, feature = "checked-handles"))]
#[derive(Default)]
struct PageGens {
    gens: Box<[u32]>,
    floor: u32,
}

#[cfg(any(debug_assertions, feature = "checked-handles"))]
#[derive(Default)]
struct Gens(Vec<PageGens>);

#[cfg(any(debug_assertions, feature = "checked-handles"))]
impl Gens {
    #[inline(always)]
    fn of(&self, page: usize, slot: usize) -> Option<Gen> {
        self.0.get(page)?.gens.get(slot).map(|&g| Gen(g))
    }
    fn on_new_page(&mut self, page: usize, slots: usize) {
        if page == self.0.len() {
            self.0.push(PageGens::default());
        }
        let p = &mut self.0[page];
        p.gens = vec![p.floor; slots].into_boxed_slice();
    }
    fn on_release(&mut self, page: usize) {
        let p = &mut self.0[page];
        p.floor = p
            .gens
            .iter()
            .copied()
            .max()
            .unwrap_or(p.floor)
            .wrapping_add(1);
        p.gens = Box::default();
    }
    fn on_take(&mut self, page: usize, slot: usize) {
        let g = &mut self.0[page].gens[slot];
        *g = g.wrapping_add(1);
    }
}

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
#[derive(Default)]
struct Gens;

#[cfg(not(any(debug_assertions, feature = "checked-handles")))]
impl Gens {
    #[inline(always)]
    fn of(&self, _: usize, _: usize) -> Option<Gen> {
        Some(Gen)
    }
    #[inline(always)]
    fn on_new_page(&mut self, _: usize, _: usize) {}
    #[inline(always)]
    fn on_release(&mut self, _: usize) {}
    #[inline(always)]
    fn on_take(&mut self, _: usize, _: usize) {}
}

/// Storage for every object of one type.
///
/// Slots live in fixed-size pages, sized by
/// [`Params::page_bytes`](crate::Params::page_bytes). Pages never move, so
/// growing an arena never copies live objects and never doubles peak memory,
/// unlike a single `Vec`. A page whose objects have all been collected is
/// released back to the allocator. Colors, free lists and counts are kept by
/// the collector, not here, so an object can be read while the collector
/// writes colors.
///
/// Arenas are created by [`space!`](crate::space) and only reached through a
/// [`Heap`](crate::Heap).
pub struct Arena<T> {
    /// A released page is an empty slice, which costs no allocation and
    /// makes any handle into it fail its bounds check.
    pages: Vec<Box<[Option<T>]>>,
    shift: u32,
    /// Slots per page minus one.
    mask: u32,
    gens: Gens,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            pages: Vec::new(),
            shift: DEFAULT_PAGE_SHIFT,
            mask: (1 << DEFAULT_PAGE_SHIFT) - 1,
            gens: Gens::default(),
        }
    }
}

impl<T> fmt::Debug for Arena<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arena")
            .field(
                "pages",
                &self.pages.iter().filter(|p| !p.is_empty()).count(),
            )
            .field("slots_per_page", &(1usize << self.shift))
            .finish()
    }
}

/// Slots per page before the heap configures the arena.
const DEFAULT_PAGE_SHIFT: u32 = 10;
/// Pages hold between 2^4 and 2^16 slots, so an in-page offset fits a `u16`.
pub(crate) const MIN_PAGE_SHIFT: u32 = 4;
pub(crate) const MAX_PAGE_SHIFT: u32 = 16;

/// The page shift that gives pages of at least `page_bytes` for slots of
/// `slot_bytes`: the slot count is rounded up to a power of two, so indexing
/// stays a shift and a mask.
pub(crate) fn page_shift(page_bytes: usize, slot_bytes: usize) -> u32 {
    let slots = page_bytes.div_ceil(slot_bytes.max(1)).max(1);
    slots
        .next_power_of_two()
        .trailing_zeros()
        .clamp(MIN_PAGE_SHIFT, MAX_PAGE_SHIFT)
}

#[cold]
#[inline(never)]
#[track_caller]
fn stale(i: u32) -> ! {
    panic!("orbit-gc: stale handle (slot {i} was freed)")
}

impl<T> Arena<T> {
    #[inline(always)]
    fn split(&self, i: u32) -> (usize, usize) {
        ((i >> self.shift) as usize, (i & self.mask) as usize)
    }

    #[inline(always)]
    #[track_caller]
    fn slot(&self, i: u32) -> &Option<T> {
        let (p, o) = self.split(i);
        match self.pages.get(p).and_then(|page| page.get(o)) {
            Some(s) => s,
            None => stale(i),
        }
    }

    #[inline(always)]
    #[track_caller]
    fn slot_mut(&mut self, i: u32) -> &mut Option<T> {
        let (p, o) = self.split(i);
        match self.pages.get_mut(p).and_then(|page| page.get_mut(o)) {
            Some(s) => s,
            None => stale(i),
        }
    }

    #[inline(always)]
    #[track_caller]
    fn check(&self, gc: Gc<T>) {
        let (p, o) = self.split(gc.index);
        if self.gens.of(p, o) != Some(gc.generation) {
            stale(gc.index)
        }
    }

    /// The object `gc` points to. Panics if its slot is free or, with checked
    /// handles, was freed and reused since the handle was made.
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get(&self, gc: Gc<T>) -> &T {
        self.check(gc);
        self.get_slot(gc.index)
    }

    #[inline(always)]
    #[track_caller]
    pub(crate) fn get_mut(&mut self, gc: Gc<T>) -> &mut T {
        self.check(gc);
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
    #[track_caller]
    pub(crate) fn handle_at(&self, i: u32) -> Gc<T> {
        let (p, o) = self.split(i);
        match self.gens.of(p, o) {
            Some(generation) => Gc {
                index: i,
                generation,
                _t: PhantomData,
            },
            None => stale(i),
        }
    }

    pub(crate) fn is_occupied(&self, gc: Gc<T>) -> bool {
        let (p, o) = self.split(gc.index);
        self.gens.of(p, o) == Some(gc.generation)
            && self
                .pages
                .get(p)
                .and_then(|page| page.get(o))
                .is_some_and(Option::is_some)
    }

    /// Set the page size. Only valid before the first allocation.
    #[doc(hidden)]
    pub fn set_shift(&mut self, shift: u32) {
        assert!(
            self.pages.is_empty(),
            "orbit-gc: page size set after allocation"
        );
        self.shift = shift;
        self.mask = (1 << shift) - 1;
    }

    /// Store `v` in slot `i`, allocating its page if it is new or was
    /// released. The collector decides which pages exist.
    pub(crate) fn put(&mut self, i: u32, v: T) -> Gc<T> {
        let (p, o) = self.split(i);
        if self.pages.get(p).is_none_or(|page| page.is_empty()) {
            let slots = 1usize << self.shift;
            if p == self.pages.len() {
                self.pages.push(Box::default());
            }
            self.pages[p] = (0..slots).map(|_| None).collect();
            self.gens.on_new_page(p, slots);
        }
        self.pages[p][o] = Some(v);
        self.handle_at(i)
    }

    pub(crate) fn take(&mut self, i: u32) -> T {
        let (p, o) = self.split(i);
        self.gens.on_take(p, o);
        match self.slot_mut(i).take() {
            Some(v) => v,
            None => stale(i),
        }
    }

    /// Give page `p`'s memory back to the allocator. Every slot must be free.
    #[doc(hidden)]
    pub fn release_page(&mut self, p: u32) {
        let page = std::mem::take(&mut self.pages[p as usize]);
        debug_assert!(page.iter().all(Option::is_none));
        drop(page);
        self.gens.on_release(p as usize);
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
