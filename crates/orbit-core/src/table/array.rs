//! The array part's storage. The raw words and their layout stay in this
//! module; its API takes and returns `Value`s, typed `Tag`s and opaque
//! `Payload`s.
//!
//! Layout (ADR-1, grouped): one allocation of [`Group`]s, each [`GROUP`]
//! payload words followed by those slots' tag bytes, 72 bytes for 8 slots
//! (9 bytes per slot). Slot `i` is entry `i % 8` of group `i / 8`, so one
//! bounds check on the group covers its payload and its tag. The capacity
//! is always a whole number of groups, and growing appends zeroed groups,
//! so no group ever moves.
//!
//! Slots below `len` may hold nil (holes). The tag bytes of slots from
//! `len` up are always nil, so a slot whose tag is not nil is below `len`:
//! [`ArrayPart::live`] relies on that to skip the length check. Payloads
//! of slots from `len` up are never read. Tag bytes are only ever written
//! from a [`Tag`].

use crate::value::{Payload, Tag};
use crate::{NoMemory, Value};

/// Slots per group.
const GROUP: usize = 1;

/// Eight slots: their payloads, then their tag bytes (ADR-1).
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
struct Group {
    payloads: [[u8; 8]; GROUP],
    tags: [u8; GROUP],
}

const _: () = assert!(std::mem::size_of::<Group>() == 9 * GROUP);

/// Groups for capacity `cap` (rounded up to whole groups).
fn groups_for(cap: usize) -> usize {
    cap.div_ceil(GROUP)
}

#[derive(Debug, Default)]
pub(super) struct ArrayPart {
    groups: Box<[Group]>,
    len: u32,
}

impl ArrayPart {
    pub(super) fn with_capacity(cap: usize) -> Result<ArrayPart, NoMemory> {
        if cap == 0 {
            return Ok(ArrayPart::default());
        }
        assert!(cap <= u32::MAX as usize, "table overflow");
        let n = groups_for(cap);
        let mut groups = Vec::new();
        groups.try_reserve_exact(n)?;
        groups.resize(n, Group::default());
        Ok(ArrayPart {
            groups: groups.into_boxed_slice(),
            len: 0,
        })
    }

    #[inline(always)]
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    #[inline(always)]
    pub(super) fn cap(&self) -> usize {
        self.groups.len() * GROUP
    }

    /// The tag byte and payload of slot `idx` when it holds a value (its
    /// tag is not nil); `None` for a nil slot or one past the array part.
    /// A non-nil tag means `idx < len` (see the module's invariant), so
    /// the one bounds check on the group is all this needs.
    #[inline(always)]
    pub(super) fn live(&self, idx: u64) -> Option<(u8, Payload)> {
        let g = self.groups.get(usize::try_from(idx / GROUP as u64).ok()?)?;
        let k = (idx % GROUP as u64) as usize;
        let byte = g.tags[k];
        if byte == Tag::Nil as u8 {
            return None;
        }
        Some((byte, Payload::from_bits(u64::from_ne_bytes(g.payloads[k]))))
    }

    #[inline(always)]
    pub(super) fn tag(&self, i: usize) -> Tag {
        let byte = self.groups[i / GROUP].tags[i % GROUP];
        // Every tag byte was written from a `Tag`, so the fallback never
        // happens; it folds into `unpack`'s match where `expect` would add
        // a branch to every array read.
        Tag::from_u8(byte).unwrap_or(Tag::Nil)
    }

    #[inline(always)]
    pub(super) fn get(&self, i: usize) -> Value {
        let g = &self.groups[i / GROUP];
        let tag = Tag::from_u8(g.tags[i % GROUP]).unwrap_or(Tag::Nil);
        Value::unpack(
            tag,
            Payload::from_bits(u64::from_ne_bytes(g.payloads[i % GROUP])),
        )
    }

    #[inline(always)]
    pub(super) fn set(&mut self, i: usize, v: Value) {
        let (t, p) = v.pack();
        let g = &mut self.groups[i / GROUP];
        g.payloads[i % GROUP] = p.bits().to_ne_bytes();
        g.tags[i % GROUP] = t as u8;
    }

    #[inline(always)]
    pub(super) fn push(&mut self, v: Value) -> Result<(), NoMemory> {
        if self.len() == self.cap() {
            self.grow_one()?;
        }
        self.len += 1;
        self.set(self.len() - 1, v);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn grow_one(&mut self) -> Result<(), NoMemory> {
        self.realloc(self.grown(self.len() + 1))
    }

    /// The capacity pushing up to `n` slots grows to: doubling, from one
    /// group.
    pub(super) fn grown(&self, n: usize) -> usize {
        let mut cap = self.cap();
        while cap < n {
            cap = (cap * 2).max(GROUP.max(4));
        }
        cap
    }

    /// Grow to room for `cap` slots (more than now), rounded up to whole
    /// groups. The allocation is resized in place where the allocator can;
    /// no group moves, and the new groups start zeroed, so every new tag
    /// byte reads nil. On failure nothing changes.
    pub(super) fn realloc(&mut self, cap: usize) -> Result<(), NoMemory> {
        assert!(cap <= u32::MAX as usize, "table overflow");
        let mut v = std::mem::take(&mut self.groups).into_vec();
        let total = groups_for(cap);
        if v.try_reserve_exact(total - v.len()).is_err() {
            // A boxed slice's vector is exactly full, so this gives the
            // same allocation back.
            self.groups = v.into_boxed_slice();
            return Err(NoMemory);
        }
        v.resize(total, Group::default());
        self.groups = v.into_boxed_slice();
        Ok(())
    }

    /// How many of slots `lo..hi` (all below `len`) are non-nil.
    pub(super) fn count_present(&self, lo: usize, hi: usize) -> usize {
        (lo..hi)
            .filter(|&i| self.groups[i / GROUP].tags[i % GROUP] != Tag::Nil as u8)
            .count()
    }

    /// Lengthen to `len` slots (within the capacity), the new ones nil.
    pub(super) fn extend_nil(&mut self, len: usize) {
        debug_assert!(len <= self.cap());
        for i in self.len()..len {
            self.set(i, Value::Nil);
        }
        self.len = len as u32;
    }

    pub(super) fn bytes(&self) -> usize {
        self.groups.len() * std::mem::size_of::<Group>()
    }

    /// The bytes an array part of capacity `cap` takes.
    pub(super) fn bytes_for(cap: usize) -> usize {
        groups_for(cap) * std::mem::size_of::<Group>()
    }

    /// The invariant [`ArrayPart::live`] relies on: every tag byte from
    /// `len` up is nil.
    #[cfg(test)]
    pub(super) fn tags_past_len_are_nil(&self) -> bool {
        (self.len()..self.cap()).all(|i| self.groups[i / GROUP].tags[i % GROUP] == Tag::Nil as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(i: usize) -> Value {
        match i % 4 {
            0 => Value::Bool(i % 8 == 0),
            1 => Value::Int(i as i64),
            2 => Value::Float(i as f64 + 0.5),
            _ => Value::Nil,
        }
    }

    fn check(a: &ArrayPart, what: &str) {
        assert_eq!(a.bytes(), ArrayPart::bytes_for(a.cap()), "{what}");
        assert!(a.tags_past_len_are_nil(), "{what}");
        for i in 0..a.len() {
            assert!(a.get(i).same(val(i)), "{what}: slot {i}");
            let live = a.live(i as u64);
            assert_eq!(live.is_some(), !val(i).is_nil(), "{what}: slot {i}");
            if let Some((t, p)) = live {
                assert!(
                    Value::unpack(Tag::from_u8(t).unwrap(), p).same(val(i)),
                    "{what}: slot {i}"
                );
            }
        }
        for i in a.len()..a.cap() + 2 * GROUP {
            assert!(a.live(i as u64).is_none(), "{what}: slot {i}");
        }
        for idx in [u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX] {
            assert!(a.live(idx).is_none(), "{what}: {idx}");
        }
    }

    #[test]
    fn sizes_are_whole_groups() {
        for cap in 0..100 {
            assert_eq!(ArrayPart::bytes_for(cap), cap.div_ceil(GROUP) * 9 * GROUP);
        }
    }

    #[test]
    fn pushes_across_group_boundaries() {
        for presize in [0, 1, 3, 4, 7, 8, 9, 15, 16, 17] {
            let mut a = ArrayPart::with_capacity(presize).unwrap();
            check(&a, "new");
            for i in 0..70 {
                a.push(val(i)).unwrap();
                check(&a, &format!("presize {presize}, push {i}"));
            }
        }
    }

    #[test]
    fn realloc_and_extend_across_group_boundaries() {
        for (from, to) in [(1, 3), (3, 8), (4, 9), (5, 17), (8, 16), (9, 24), (12, 100)] {
            let mut a = ArrayPart::with_capacity(from).unwrap();
            for i in 0..from {
                a.push(val(i)).unwrap();
            }
            a.realloc(to).unwrap();
            check(&a, &format!("{from} -> {to}"));
            // Lengthen with nil slots as a rebuild does, then fill them.
            a.extend_nil(to);
            assert!(a.tags_past_len_are_nil());
            for i in from..to {
                assert!(a.get(i).is_nil());
                a.set(i, val(i));
            }
            check(&a, &format!("{from} -> {to}, filled"));
            assert_eq!(
                a.count_present(0, to),
                (0..to).filter(|&i| !val(i).is_nil()).count()
            );
            // Clearing slots (as weak tables and stores of nil do) leaves
            // them below `len`, reading as absent.
            for i in (0..to).step_by(3) {
                a.set(i, Value::Nil);
                assert!(a.live(i as u64).is_none());
            }
            assert!(a.tags_past_len_are_nil());
        }
    }
}
