//! Recycling of small table parts.
//!
//! A collection frees the hash and array parts of dead tables in a batch,
//! many more than the system allocator's per-thread cache holds, and
//! allocating from its slower lists afterwards cost a table constructor more
//! than building the table. Small parts dropped with their table go to a
//! per-thread pool instead, bounded to [`POOL_BYTES`], and new parts of
//! those sizes come from it first. Pooled parts hold no handles: they are
//! cleared when taken, and the collector never sees them.

use std::cell::RefCell;
use std::mem::size_of_val;

use super::Entry;
use super::array::Slot;

/// Bytes of parts a thread's pool keeps at most.
const POOL_BYTES: usize = 64 * 1024;

/// Hash parts of 1, 2, 4 and 8 slots.
const ENTRY_CLASSES: usize = 4;
/// Array parts of 1 to 8 slots.
const SLOT_CLASSES: usize = 8;

#[derive(Default)]
pub(super) struct Pool {
    entries: [Vec<Box<[Entry]>>; ENTRY_CLASSES],
    slots: [Vec<Box<[Slot]>>; SLOT_CLASSES],
    bytes: usize,
}

thread_local! {
    static POOL: RefCell<Pool> = RefCell::new(Pool::default());
}

/// An element type of a pooled part.
pub(super) trait Part: Clone + Default + Sized {
    /// The pool class of a part of `cap` elements, if it has one.
    fn class(cap: usize) -> Option<usize>;
    fn list(pool: &mut Pool, class: usize) -> &mut Vec<Box<[Self]>>;
}

impl Part for Entry {
    #[inline(always)]
    fn class(cap: usize) -> Option<usize> {
        (cap.is_power_of_two() && cap < 1 << ENTRY_CLASSES).then(|| cap.trailing_zeros() as usize)
    }

    #[inline(always)]
    fn list(pool: &mut Pool, class: usize) -> &mut Vec<Box<[Entry]>> {
        &mut pool.entries[class]
    }
}

impl Part for Slot {
    #[inline(always)]
    fn class(cap: usize) -> Option<usize> {
        (1..=SLOT_CLASSES).contains(&cap).then(|| cap - 1)
    }

    #[inline(always)]
    fn list(pool: &mut Pool, class: usize) -> &mut Vec<Box<[Slot]>> {
        &mut pool.slots[class]
    }
}

/// A part of `cap` default elements, from the pool when it has one.
#[inline]
pub(super) fn take<T: Part>(cap: usize) -> Option<Box<[T]>> {
    let class = T::class(cap)?;
    let mut part = POOL
        .try_with(|p| {
            let mut p = p.try_borrow_mut().ok()?;
            let part = T::list(&mut p, class).pop()?;
            p.bytes -= size_of_val(&*part);
            Some(part)
        })
        .ok()
        .flatten()?;
    part.fill(T::default());
    Some(part)
}

/// Keep `part` for reuse if it is small and the pool has room; otherwise
/// it is freed.
#[inline]
fn give<T: Part>(part: Box<[T]>) {
    let Some(class) = T::class(part.len()) else {
        return;
    };
    // During thread exit the pool may be gone: the part is just freed.
    let _ = POOL.try_with(|p| {
        if let Ok(mut p) = p.try_borrow_mut() {
            let bytes = size_of_val(&*part);
            if p.bytes + bytes <= POOL_BYTES {
                p.bytes += bytes;
                T::list(&mut p, class).push(part);
            }
        }
    });
}

/// An owned part that goes back to the pool when dropped.
#[derive(Debug, Default)]
pub(super) struct Recycled<T: Part>(pub(super) Box<[T]>);

impl<T: Part> Drop for Recycled<T> {
    fn drop(&mut self) {
        give(std::mem::take(&mut self.0));
    }
}

impl<T: Part> std::ops::Deref for Recycled<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T: Part> std::ops::DerefMut for Recycled<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_come_back_cleared_and_only_small_ones() {
        use crate::object::Value;
        let used = Entry::new(Value::Int(1), 5, Value::Int(2));
        drop(Recycled(vec![used.clone(); 2].into_boxed_slice()));
        let part: Box<[Entry]> = take(2).expect("a pooled part");
        assert_eq!(part.len(), 2);
        assert!(part.iter().all(|e| e.is_empty() && e.value_is_nil()));
        // Hash parts pool by power-of-two size only, up to 8 slots.
        drop(Recycled(vec![used.clone(); 16].into_boxed_slice()));
        assert!(take::<Entry>(16).is_none());
        assert!(take::<Entry>(3).is_none());
        // Array parts pool by exact size, up to 8 slots.
        drop(Recycled(vec![Slot::default(); 3].into_boxed_slice()));
        assert_eq!(take::<Slot>(3).map(|p| p.len()), Some(3));
        drop(Recycled(vec![Slot::default(); 9].into_boxed_slice()));
        assert!(take::<Slot>(9).is_none());
    }
}
