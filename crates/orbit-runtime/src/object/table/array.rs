//! The array part's storage (ADR-1). The raw bytes and their layout stay
//! in this module; its API takes and returns `Value`s, typed `Tag`s and
//! opaque `Payload`s.
//!
//! Layout: one allocation of [`Slot`]s, 9 bytes each: the 8-byte payload,
//! then the tag byte. Slot `i` starts at byte `9 * i`, so one bounds check
//! on the slot covers its payload and its tag, and no capacity has to be
//! loaded to find the tags. Growing appends zeroed slots; no slot moves.
//!
//! Slots below `len` may hold nil (holes). The tag bytes of slots from
//! `len` up are always nil, so a slot whose tag is not nil is below `len`:
//! [`ArrayPart::live`] relies on that to skip the length check. Payloads
//! of slots from `len` up are never read. Tag bytes are only ever written
//! from a [`Tag`].

use super::pool::{self, Recycled};
use crate::object::value::{Payload, Tag};
use crate::object::{NoMemory, Value};

/// One array slot. The payload is a byte array so a slot is 9 bytes, not
/// 16: payloads are read and written unaligned, which x86-64 and aarch64
/// do in one instruction.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub(super) struct Slot {
    payload: [u8; 8],
    tag: u8,
}

const _: () = assert!(std::mem::size_of::<Slot>() == 9);

impl Slot {
    #[inline(always)]
    fn payload(&self) -> Payload {
        Payload::from_bits(u64::from_ne_bytes(self.payload))
    }
}

#[derive(Debug, Default)]
pub(super) struct ArrayPart {
    slots: Recycled<Slot>,
    len: u32,
    /// Where the last nil was stored by [`ArrayPart::set_nil_hinted`]: a
    /// guess at the border once the last slot is nil. Only a hint, checked
    /// before use; it fits in the padding after `len`.
    nil_hint: u32,
}

impl ArrayPart {
    pub(super) fn with_capacity(cap: usize) -> Result<ArrayPart, NoMemory> {
        if cap == 0 {
            return Ok(ArrayPart::default());
        }
        assert!(cap <= u32::MAX as usize, "table overflow");
        let slots = match pool::take(cap) {
            Some(slots) => slots,
            None => {
                let mut slots = Vec::new();
                slots.try_reserve_exact(cap)?;
                slots.resize(cap, Slot::default());
                slots.into_boxed_slice()
            }
        };
        Ok(ArrayPart {
            slots: Recycled(slots),
            len: 0,
            nil_hint: 0,
        })
    }

    #[inline(always)]
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    #[inline(always)]
    pub(super) fn cap(&self) -> usize {
        self.slots.len()
    }

    /// The tag byte and payload of slot `idx` when it holds a value (its
    /// tag is not nil); `None` for a nil slot or one past the array part.
    /// A non-nil tag means `idx < len` (see the module's invariant), so
    /// the slot's one bounds check is all this needs.
    #[inline(always)]
    pub(super) fn live(&self, idx: u64) -> Option<(u8, Payload)> {
        let s = self.slots.get(usize::try_from(idx).ok()?)?;
        if s.tag == Tag::Nil as u8 {
            return None;
        }
        Some((s.tag, s.payload()))
    }

    #[inline(always)]
    pub(super) fn tag(&self, i: usize) -> Tag {
        // Every tag byte was written from a `Tag`, so the fallback never
        // happens; it folds into `unpack`'s match where `expect` would add
        // a branch to every array read.
        Tag::from_u8(self.slots[i].tag).unwrap_or(Tag::Nil)
    }

    #[inline(always)]
    pub(super) fn get(&self, i: usize) -> Value {
        let s = &self.slots[i];
        Value::unpack(Tag::from_u8(s.tag).unwrap_or(Tag::Nil), s.payload())
    }

    #[inline(always)]
    pub(super) fn set(&mut self, i: usize, v: Value) {
        let (t, p) = v.pack();
        self.slots[i] = Slot {
            payload: p.bits().to_ne_bytes(),
            tag: t as u8,
        };
    }

    /// Stores nil in slot `i` and remembers it as a border candidate.
    #[inline(always)]
    pub(super) fn set_nil_hinted(&mut self, i: usize) {
        self.set(i, Value::Nil);
        self.nil_hint = i as u32;
    }

    /// A border inside the array part when its last slot is nil: `b` with
    /// slot `b` nil and slot `b - 1` not (or `b = 0`). A table used as a
    /// stack moves its border one slot at a time, so the slot of the last
    /// nil stored and the one after it are tried before bisecting.
    pub(super) fn inner_border(&self) -> usize {
        let n = self.len();
        debug_assert!(n > 0 && self.tag(n - 1) == Tag::Nil);
        let is_border =
            |b: usize| b < n && self.tag(b) == Tag::Nil && (b == 0 || self.tag(b - 1) != Tag::Nil);
        let h = self.nil_hint as usize;
        for b in [h, h + 1] {
            if is_border(b) {
                return b;
            }
        }
        // Slot `lo - 1` non-nil (or `lo = 0`), slot `hi - 1` nil.
        let (mut lo, mut hi) = (0usize, n);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.tag(mid - 1) == Tag::Nil {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        lo
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

    /// Appends `v` if there is spare capacity, without allocating. Returns
    /// whether it did.
    #[inline(always)]
    pub(super) fn push_in_place(&mut self, v: Value) -> bool {
        if self.len() == self.cap() {
            return false;
        }
        self.len += 1;
        self.set(self.len() - 1, v);
        true
    }

    #[cold]
    #[inline(never)]
    fn grow_one(&mut self) -> Result<(), NoMemory> {
        self.realloc(self.grown(self.len() + 1))
    }

    /// The capacity pushing up to `n` slots grows to: doubling, from 4.
    pub(super) fn grown(&self, n: usize) -> usize {
        let mut cap = self.cap();
        while cap < n {
            cap = (cap * 2).max(4);
        }
        cap
    }

    /// Grow to room for `cap` slots (more than now). The allocation is
    /// resized in place where the allocator can; no slot moves, and the
    /// new slots start zeroed, so every new tag byte reads nil. On failure
    /// nothing changes.
    pub(super) fn realloc(&mut self, cap: usize) -> Result<(), NoMemory> {
        assert!(cap <= u32::MAX as usize, "table overflow");
        let mut v = std::mem::take(&mut self.slots.0).into_vec();
        if v.try_reserve_exact(cap - v.len()).is_err() {
            // A boxed slice's vector is exactly full, so this gives the
            // same allocation back.
            self.slots.0 = v.into_boxed_slice();
            return Err(NoMemory);
        }
        v.resize(cap, Slot::default());
        self.slots.0 = v.into_boxed_slice();
        Ok(())
    }

    /// How many of slots `lo..hi` (all below `len`) are non-nil.
    pub(super) fn count_present(&self, lo: usize, hi: usize) -> usize {
        self.slots[lo..hi]
            .iter()
            .filter(|s| s.tag != Tag::Nil as u8)
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

    /// The bytes this array part takes, for the collector's accounting.
    pub(super) fn bytes(&self) -> usize {
        Self::bytes_for(self.cap())
    }

    /// The bytes an array part of capacity `cap` takes, rounded up to the
    /// allocator's 8-byte granularity (the GC paces on this; counting a
    /// 2-slot part as 18 bytes when malloc spends 24 or more on it let
    /// binary_trees' heap grow 4% further between collections).
    pub(super) fn bytes_for(cap: usize) -> usize {
        (cap * std::mem::size_of::<Slot>()).next_multiple_of(8)
    }

    /// The invariant [`ArrayPart::live`] relies on: every tag byte from
    /// `len` up is nil.
    #[cfg(test)]
    pub(super) fn tags_past_len_are_nil(&self) -> bool {
        self.slots[self.len()..]
            .iter()
            .all(|s| s.tag == Tag::Nil as u8)
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
        for i in a.len()..a.cap() + 16 {
            assert!(a.live(i as u64).is_none(), "{what}: slot {i}");
        }
        for idx in [u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX] {
            assert!(a.live(idx).is_none(), "{what}: {idx}");
        }
    }

    #[test]
    fn sizes_are_no_more_than_payloads_then_tags() {
        for cap in 0..100 {
            // 9 bytes a slot, never more than the layout before slots.
            assert!(ArrayPart::bytes_for(cap) >= cap * 9);
            assert!(ArrayPart::bytes_for(cap) <= cap * 8 + cap.div_ceil(8) * 8);
        }
        assert_eq!(ArrayPart::bytes_for(2), 24);
        assert_eq!(ArrayPart::bytes_for(4), 40);
        assert_eq!(ArrayPart::bytes_for(8), 72);
    }

    #[test]
    fn pushes_keep_every_slot() {
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
    fn realloc_and_extend_keep_every_slot() {
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
