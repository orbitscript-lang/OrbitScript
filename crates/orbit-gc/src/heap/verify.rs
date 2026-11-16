//! The heap verifier, compiled into debug builds only.
//!
//! When marking ends, it walks the heap again from the roots, through the
//! same [`Trace`](crate::Trace) code the collector uses, and requires every
//! object it reaches to be marked and every handle it follows to be current.
//! An object reachable but unmarked is about to be freed while in use: the
//! usual cause is a store into an object the collector had already traced
//! without a write barrier. The panic names the object and the chain of
//! objects that leads to it from a root, so the store to look for is one
//! into the object just before it.
//!
//! It also adds up the bytes of every object in the heap and compares the
//! total with [`Heap::bytes`]: a change to
//! [`Trace::extra_bytes`](crate::Trace::extra_bytes) that was not reported
//! ([`Heap::update`] or [`Heap::adjust_bytes`]) shows up there.
//!
//! What it cannot see: a `trace` that forgets a handle misleads the
//! collector and the verifier alike.

use super::{Heap, Verify};
use crate::arena::AnyGc;
use crate::trace::{COLOR, FREE, KindMeta, Marker, Roots, Space, WHITE};
use std::collections::HashMap;
use std::fmt::Write as _;

/// How the walk reached an object, in [`Check::reached`]: not yet, from a
/// root, or from another object (its kind and slot packed, plus one).
const UNREACHED: u64 = 0;
const ROOT: u64 = u64::MAX;

fn pack(kind: u8, index: u32) -> u64 {
    (u64::from(kind) << 32 | u64::from(index)) + 1
}

fn unpack(from: u64) -> (u8, u32) {
    let v = from - 1;
    ((v >> 32) as u8, v as u32)
}

/// The verifier's state, kept in [`Marks`](crate::trace::Marks) so the
/// [`Marker`] the `Trace` code calls reports to it while a walk is on.
#[derive(Debug, Default)]
pub(crate) struct Check {
    /// Whether a walk is in progress: [`Marker`] then reports here instead
    /// of marking.
    pub(crate) active: bool,
    /// Per kind, per slot: how the walk reached the object (see
    /// [`UNREACHED`]). Kept between walks for its buffers.
    reached: Vec<Vec<u64>>,
    /// Objects reached and not yet traced.
    todo: Vec<(u8, u32)>,
    /// Ephemeron values whose key the walk has reached since they were
    /// reported, with the object that holds the entry.
    released: Vec<(AnyGc, u64)>,
    /// Ephemeron values whose key the walk has not reached yet, by key,
    /// each with the object that holds the entry.
    waiting: HashMap<(u8, u32), Vec<(AnyGc, u64)>>,
    /// The object being traced, as in `reached`, or [`ROOT`].
    from: u64,
    /// What kind of collection is being checked, for the report.
    context: String,
    /// Set (with `active`) while a write guard lists an object's handles:
    /// [`Marker`] then only collects what it is shown.
    pub(crate) collecting: bool,
    /// The handles collected.
    pub(crate) collected: Vec<AnyGc>,
}

impl Check {
    fn begin(&mut self, meta: &[KindMeta], context: String) {
        self.reached.resize_with(meta.len(), Vec::new);
        for (r, m) in self.reached.iter_mut().zip(meta) {
            r.clear();
            r.resize(m.color.len(), UNREACHED);
        }
        self.todo.clear();
        self.released.clear();
        self.waiting.clear();
        self.from = ROOT;
        self.context = context;
        self.active = true;
    }

    fn end(&mut self) {
        self.active = false;
        self.waiting.clear();
        // A big heap's buffers are not kept for the next walk.
        if self.reached.iter().map(Vec::capacity).sum::<usize>() > 1 << 20 {
            self.reached = Vec::new();
        }
    }

    /// The walk reached `index` of `kind` from the object being traced.
    /// `current` is false when the handle is known to be stale (its slot
    /// was freed, and maybe reused).
    pub(crate) fn reach<S: Space>(
        &mut self,
        meta: &[KindMeta],
        kind: u8,
        index: u32,
        current: bool,
        leaf: bool,
    ) {
        let color = meta[kind as usize]
            .color
            .get(index as usize)
            .copied()
            .unwrap_or(FREE);
        if !current || color == FREE {
            self.fail::<S>(kind, index, "was freed: the handle to it is stale");
        }
        let slot = &mut self.reached[kind as usize][index as usize];
        if *slot != UNREACHED {
            return;
        }
        *slot = self.from;
        if color & COLOR == WHITE {
            self.fail::<S>(kind, index, "is not marked, so it would be freed");
        }
        if !leaf {
            self.todo.push((kind, index));
        }
        if let Some(values) = self.waiting.remove(&(kind, index)) {
            self.released.extend(values);
        }
    }

    /// An ephemeron entry: `value` is reachable once `key` is.
    pub(crate) fn ephemeron<S: Space>(&mut self, meta: &[KindMeta], key: AnyGc, value: AnyGc) {
        let reached = self.reached[key.kind as usize]
            .get(key.index as usize)
            .is_some_and(|&r| r != UNREACHED);
        if reached {
            self.reach::<S>(meta, value.kind, value.index, true, false);
        } else {
            self.waiting
                .entry((key.kind, key.index))
                .or_default()
                .push((value, self.from));
        }
    }

    fn describe<S: Space>(kind: u8, index: u32) -> String {
        format!("{}#{index}", S::kind_name(kind))
    }

    /// Panic with the chain of objects from a root to `index` of `kind`.
    #[cold]
    #[inline(never)]
    fn fail<S: Space>(&self, kind: u8, index: u32, problem: &str) -> ! {
        // The links nearest the object are listed, at most this many.
        const SHOWN: usize = 24;
        let mut chain = vec![Self::describe::<S>(kind, index)];
        let mut skipped = 0usize;
        let mut from = self.from;
        while from != ROOT {
            let (k, i) = unpack(from);
            if chain.len() < SHOWN {
                chain.push(Self::describe::<S>(k, i));
            } else {
                skipped += 1;
            }
            from = self.reached[k as usize][i as usize];
        }
        let holder = chain.get(1).cloned();
        chain.reverse();
        let mut path = String::from("root");
        if skipped > 0 {
            let _ = write!(path, " -> ({skipped} more)");
        }
        for link in &chain {
            let _ = write!(path, " -> {link}");
        }
        let cause = match holder {
            Some(h) => format!(
                "Look for a store into {h} without a write barrier, or with a safe point between the barrier and the store"
            ),
            None => "It is held by a root".to_string(),
        };
        panic!(
            "orbit-gc verify ({}): {} {problem}. Reached by: {path}. {cause}.",
            self.context,
            Self::describe::<S>(kind, index),
        );
    }
}

impl<S: Space> Marker<'_, S> {
    /// Trace every object the verifier's walk reaches.
    fn walk(&mut self) {
        loop {
            if let Some((v, holder)) = self.marks.check.released.pop() {
                self.marks.check.from = holder;
                let marks = &mut *self.marks;
                marks
                    .check
                    .reach::<S>(&marks.meta, v.kind, v.index, true, false);
            } else if let Some((kind, index)) = self.marks.check.todo.pop() {
                self.marks.check.from = pack(kind, index);
                self.space.trace_slot(kind, index, self);
            } else {
                break;
            }
        }
    }
}

impl<S: Space> Heap<S> {
    /// Run the verifier if it is on: at the end of every marking, before
    /// anything is swept.
    pub(super) fn verify<R: Roots<S> + ?Sized>(&mut self, roots: &R, minor: bool) {
        let due = match self.verifier {
            Verify::Off => false,
            Verify::Every => true,
            Verify::Paced => {
                !minor || self.charged.wrapping_sub(self.charged_at_check) >= self.bytes_at_check
            }
        };
        if !due {
            return;
        }
        self.charged_at_check = self.charged;
        self.bytes_at_check = self.bytes;
        let context = format!(
            "{} collection, {:?} mode",
            if minor { "minor" } else { "full" },
            self.mode
        );
        self.marks.check.begin(&self.marks.meta, context);
        let mut m = Marker {
            space: &self.space,
            seed: self.seed,
            marks: &mut self.marks,
            weak: None,
            can_register: false,
        };
        roots.trace(&mut m);
        // Objects waiting for their finalizer are roots too.
        for &gc in &self.finalize_queue {
            m.marks.check.from = ROOT;
            m.marks
                .check
                .reach::<S>(&m.marks.meta, gc.kind, gc.index, true, false);
        }
        m.walk();
        self.marks.check.end();
        self.verify_bytes();
    }

    /// Compare [`Heap::bytes`] with what the objects in the heap add up to.
    fn verify_bytes(&self) {
        let mut total = 0usize;
        for (kind, meta) in self.marks.meta.iter().enumerate() {
            for (index, &c) in meta.color.iter().enumerate() {
                if c != FREE {
                    total += self.space.slot_size_of(kind as u8, index as u32);
                }
            }
        }
        assert!(
            total == self.bytes,
            "orbit-gc verify: Heap::bytes is {} but the objects in the heap add up to {total} \
             ({:+}). A change to an object's Trace::extra_bytes was not reported: use \
             Heap::update for such changes, or Heap::adjust_bytes.",
            self.bytes,
            total as isize - self.bytes as isize,
        );
    }
}
