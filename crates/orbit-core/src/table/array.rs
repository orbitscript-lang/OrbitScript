//! The array part's storage. The raw words and their layout stay in this
//! module; its API takes and returns `Value`s, typed `Tag`s and opaque
//! `Payload`s.
//!
//! Layout (ADR-1, grouped): one allocation of 8-byte words, in groups of
//! up to [`GROUP`] slots. A group is one tag word (the slots' tag bytes, one
//! per slot, in slot order) followed by the slots' payload words, so slot
//! `i` has its tag byte at byte `i % 8` of word `9 * (i / 8)` and its
//! payload at word `9 * (i / 8) + 1 + i % 8`. Every group is full except
//! perhaps the last, which holds only the payload words of the slots below
//! the capacity: capacity `cap` takes `cap + cap.div_ceil(8)` words, 9 bytes
//! per slot, the same as payloads-then-tags. The words of a smaller
//! capacity are a prefix of a larger one's, so growing never moves a word.
//!
//! Slot `i`'s tag word comes before its payload word, so one bounds check on
//! the payload word covers both.
//!
//! Slots below `len` may hold nil (holes). The tag bytes of slots from
//! `len` up, and the unused tag bytes of a partial last group, are always
//! nil, so a slot whose tag is not nil is below `len`: [`ArrayPart::live`]
//! relies on that to skip the length check. Payload words of slots from
//! `len` up are never read. The words are byte arrays because one
//! allocation can hold only one element type; tag bytes are only ever
//! written from a [`Tag`].

use crate::value::{Payload, Tag};
use crate::{NoMemory, Value};

/// Slots per group.
const GROUP: usize = 8;

/// Word index of slot `i`'s tag word.
#[inline(always)]
fn tag_word(i: usize) -> usize {
    i / GROUP * (GROUP + 1)
}

/// Word index of slot `i`'s payload.
#[inline(always)]
fn payload_word(i: usize) -> usize {
    tag_word(i) + 1 + i % GROUP
}

/// Words for capacity `cap`.
fn words_for(cap: usize) -> usize {
    cap + cap.div_ceil(GROUP)
}

#[derive(Debug, Default)]
pub(super) struct ArrayPart {
    words: Box<[[u8; 8]]>,
    len: u32,
    cap: u32,
}

impl ArrayPart {
    pub(super) fn with_capacity(cap: usize) -> Result<ArrayPart, NoMemory> {
        if cap == 0 {
            return Ok(ArrayPart::default());
        }
        assert!(cap <= u32::MAX as usize, "table overflow");
        let total = words_for(cap);
        let mut words = Vec::new();
        words.try_reserve_exact(total)?;
        words.resize(total, [0; 8]);
        Ok(ArrayPart {
            words: words.into_boxed_slice(),
            len: 0,
            cap: cap as u32,
        })
    }

    #[inline(always)]
    pub(super) fn len(&self) -> usize {
        self.len as usize
    }

    #[inline(always)]
    pub(super) fn cap(&self) -> usize {
        self.cap as usize
    }

    /// The tag byte and payload of slot `idx` when it holds a value (its
    /// tag is not nil); `None` for a nil slot or one past the array part.
    /// A non-nil tag means `idx < len` (see the module's invariant), and
    /// the tag word comes before the payload word, so the one bounds check
    /// on the payload word is all this needs (plus a range test that keeps
    /// the word arithmetic from overflowing).
    #[inline(always)]
    pub(super) fn live(&self, idx: u64) -> Option<(u8, Payload)> {
        let i = u32::try_from(idx).ok()? as usize;
        let pw = payload_word(i);
        if pw >= self.words.len() {
            return None;
        }
        let byte = self.words[tag_word(i)][i % GROUP];
        if byte == Tag::Nil as u8 {
            return None;
        }
        Some((byte, Payload::from_bits(u64::from_ne_bytes(self.words[pw]))))
    }

    #[inline(always)]
    pub(super) fn tag(&self, i: usize) -> Tag {
        let byte = self.words[tag_word(i)][i % GROUP];
        // Every tag byte was written from a `Tag`, so the fallback never
        // happens; it folds into `unpack`'s match where `expect` would add
        // a branch to every array read.
        Tag::from_u8(byte).unwrap_or(Tag::Nil)
    }

    #[inline(always)]
    pub(super) fn get(&self, i: usize) -> Value {
        let p = Payload::from_bits(u64::from_ne_bytes(self.words[payload_word(i)]));
        Value::unpack(self.tag(i), p)
    }

    #[inline(always)]
    pub(super) fn set(&mut self, i: usize, v: Value) {
        let (t, p) = v.pack();
        self.words[payload_word(i)] = p.bits().to_ne_bytes();
        self.words[tag_word(i)][i % GROUP] = t as u8;
    }

    #[inline(always)]
    pub(super) fn push(&mut self, v: Value) -> Result<(), NoMemory> {
        if self.len == self.cap {
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

    /// The capacity pushing up to `n` slots grows to: doubling, from 4.
    pub(super) fn grown(&self, n: usize) -> usize {
        let mut cap = self.cap as usize;
        while cap < n {
            cap = (cap * 2).max(4);
        }
        cap
    }

    /// Grow to room for `cap` slots (more than now). The allocation is
    /// resized in place where the allocator can; no word moves, and the
    /// new words (tags and payloads) start zeroed, so every new tag byte
    /// reads nil. On failure nothing changes.
    pub(super) fn realloc(&mut self, cap: usize) -> Result<(), NoMemory> {
        assert!(cap <= u32::MAX as usize, "table overflow");
        let mut v = std::mem::take(&mut self.words).into_vec();
        let total = words_for(cap);
        if v.try_reserve_exact(total - v.len()).is_err() {
            // A boxed slice's vector is exactly full, so this gives the
            // same allocation back.
            self.words = v.into_boxed_slice();
            return Err(NoMemory);
        }
        v.resize(total, [0; 8]);
        self.words = v.into_boxed_slice();
        self.cap = cap as u32;
        Ok(())
    }

    /// How many of slots `lo..hi` (all below `len`) are non-nil.
    pub(super) fn count_present(&self, lo: usize, hi: usize) -> usize {
        (lo..hi)
            .filter(|&i| self.words[tag_word(i)][i % GROUP] != Tag::Nil as u8)
            .count()
    }

    /// Lengthen to `len` slots (within the capacity), the new ones nil.
    pub(super) fn extend_nil(&mut self, len: usize) {
        debug_assert!(len <= self.cap as usize);
        for i in self.len()..len {
            self.set(i, Value::Nil);
        }
        self.len = len as u32;
    }

    pub(super) fn bytes(&self) -> usize {
        self.words.len() * 8
    }

    /// The bytes an array part of capacity `cap` takes.
    pub(super) fn bytes_for(cap: usize) -> usize {
        words_for(cap) * 8
    }

    /// The invariant [`ArrayPart::live`] relies on: every tag byte from
    /// `len` up, including the unused ones of a partial last group, is nil.
    #[cfg(test)]
    pub(super) fn tags_past_len_are_nil(&self) -> bool {
        (self.len()..self.cap().next_multiple_of(GROUP)).all(|i| {
            self.words
                .get(tag_word(i))
                .is_none_or(|w| w[i % GROUP] == Tag::Nil as u8)
        })
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
    fn sizes_match_payloads_then_tags() {
        for cap in 0..100 {
            assert_eq!(ArrayPart::bytes_for(cap), cap * 8 + cap.div_ceil(8) * 8);
        }
        assert_eq!(ArrayPart::bytes_for(2), 24);
        assert_eq!(ArrayPart::bytes_for(4), 40);
        assert_eq!(ArrayPart::bytes_for(8), 72);
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
