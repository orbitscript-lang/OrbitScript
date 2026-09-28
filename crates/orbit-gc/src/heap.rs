//! The heap: allocation, barriers, pacing and the incremental collector.

use crate::arena::{AnyGc, Gc};
use crate::trace::{
    BLACK, COLOR, FIN, FREE, GRAY, Holds, KindMeta, Liveness, Marker, Marks, Roots, Space, Trace,
    WHITE, WeakMode,
};
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;

/// Collector tuning. The defaults are PUC Lua 5.5's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Params {
    /// How far the heap may grow before the next cycle starts, as a
    /// percentage of the bytes live after the last one (Lua's `pause`).
    pub pause: u32,
    /// Collector work per byte allocated, as a percentage (Lua's `stepmul`).
    pub stepmul: u32,
    /// Bytes allocated between incremental steps (Lua's `stepsize`).
    pub stepsize: usize,
    /// A floor under the threshold, so small programs collect less often.
    /// Not a Lua parameter, and 0 by default, as in Lua: with a floor, a
    /// small heap can't shrink back toward its live size.
    pub min_threshold: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            pause: 250,
            stepmul: 200,
            stepsize: 200 * 56,
            min_threshold: 0,
        }
    }
}

/// Where the collector is in its cycle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Between cycles.
    Pause,
    /// Incremental marking. Write barriers are active.
    Mark,
    /// Incremental sweeping.
    Sweep,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Pause,
    Mark,
    /// Sweeping arena `kind` from slot `at`; lower kinds are done.
    Sweep {
        kind: u8,
        at: u32,
    },
}

/// How many free slots the sweeper skips for the cost of visiting one object.
const FREE_SLOT_SHARE: usize = 16;
/// Work done by one step in stress mode.
const STRESS_WORK: usize = 256;

/// A garbage-collected heap over the arenas of space `S`.
///
/// Allocation never collects. The owner calls [`Heap::step`] at safe points
/// where every live handle is reachable from the roots it passes, typically
/// when [`Heap::should_step`] says a step is due.
pub struct Heap<S: Space> {
    space: S,
    marks: Marks,
    gray_again: Vec<AnyGc>,
    state: State,
    /// Bytes allocated minus bytes freed.
    bytes: usize,
    /// Bytes live at the end of marking, minus what the sweep has freed
    /// since: Lua's `GCestimate`. Unlike `bytes`, it leaves out what the
    /// mutator allocated during the sweep, which would otherwise feed into
    /// the next threshold and let the heap grow cycle after cycle.
    estimate: usize,
    /// Positive when a step is due.
    debt: isize,
    threshold: usize,
    params: Params,
    /// Objects that declared weak references in this cycle.
    weak: Vec<(AnyGc, WeakMode)>,
    /// Objects registered for finalization, oldest first.
    finobj: Vec<AnyGc>,
    /// Unreachable objects waiting for their finalizer, next first.
    tobefnz: VecDeque<AnyGc>,
    cycles: u64,
    stress: bool,
    stopped: bool,
}

impl<S: Space> fmt::Debug for Heap<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heap")
            .field("phase", &self.phase())
            .field("bytes", &self.bytes)
            .field("debt", &self.debt)
            .field("cycles", &self.cycles)
            .finish_non_exhaustive()
    }
}

impl<S: Space + Default> Default for Heap<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub(crate) fn slot_size<T>() -> usize {
    size_of::<Option<T>>() + 1
}

/// New objects are white, except in the part of the heap the sweeper has
/// not reached yet, where they are black so the sweeper whitens them
/// instead of freeing them.
fn alloc_color(state: State, kind: u8, index: u32) -> u8 {
    match state {
        State::Sweep { kind: k, at } if kind > k || (kind == k && index >= at) => BLACK,
        _ => WHITE,
    }
}

impl<S: Space + Default> Heap<S> {
    pub fn new() -> Self {
        Self::with_params(Params::default())
    }

    pub fn with_params(params: Params) -> Self {
        Heap {
            space: S::default(),
            marks: Marks {
                meta: (0..S::KINDS).map(|_| KindMeta::default()).collect(),
                gray: Vec::new(),
                marked: 0,
            },
            gray_again: Vec::new(),
            state: State::Pause,
            bytes: 0,
            estimate: 0,
            debt: -(params.min_threshold as isize),
            threshold: params.min_threshold,
            params,
            weak: Vec::new(),
            finobj: Vec::new(),
            tobefnz: VecDeque::new(),
            cycles: 0,
            stress: false,
            stopped: false,
        }
    }
}

impl<S: Space> Heap<S> {
    // ---- objects --------------------------------------------------------

    /// Move `v` into the heap. Never collects.
    pub fn alloc<T: Trace<S>>(&mut self, v: T) -> Gc<T>
    where
        S: Holds<T>,
    {
        let kind = <S as Holds<T>>::KIND;
        let size = slot_size::<T>() + v.extra_bytes();
        let state = self.state;
        let meta = &mut self.marks.meta[kind as usize];
        let index = match meta.free.pop() {
            Some(i) => i,
            None => {
                let i = u32::try_from(meta.color.len()).expect("orbit-gc: arena full");
                meta.color.push(FREE);
                i
            }
        };
        meta.color[index as usize] = alloc_color(state, kind, index);
        self.space.arena_mut().put(index, v);
        self.charge(size);
        Gc::from_index(index)
    }

    /// The object behind `gc`. Panics if it has been collected and its slot
    /// not reused.
    #[inline(always)]
    #[track_caller]
    pub fn get<T>(&self, gc: Gc<T>) -> &T
    where
        S: Holds<T>,
    {
        self.space.arena().get(gc.index())
    }

    /// Mutable access with no write barrier and no byte accounting. Use it
    /// for writes that store no handles and don't change
    /// [`Trace::extra_bytes`]; otherwise use [`Heap::update`], or pair it with
    /// [`Heap::barrier_back`] or [`Heap::barrier_forward`].
    #[inline(always)]
    #[track_caller]
    pub fn get_mut<T>(&mut self, gc: Gc<T>) -> &mut T
    where
        S: Holds<T>,
    {
        self.space.arena_mut().get_mut(gc.index())
    }

    /// Change an object: runs the backward write barrier, then `f`, then
    /// charges or credits any change in [`Trace::extra_bytes`].
    #[track_caller]
    pub fn update<T: Trace<S>, R>(&mut self, gc: Gc<T>, f: impl FnOnce(&mut T) -> R) -> R
    where
        S: Holds<T>,
    {
        self.barrier_back(gc);
        let obj = self.space.arena_mut().get_mut(gc.index());
        let before = obj.extra_bytes();
        let r = f(obj);
        let after = obj.extra_bytes();
        self.adjust_bytes(after as isize - before as isize);
        r
    }

    /// Whether `gc`'s slot holds an object. A collected object's slot may
    /// have been reused, so this can't prove a handle is still the object it
    /// was; it is for tests and assertions.
    pub fn is_live<T>(&self, gc: Gc<T>) -> bool
    where
        S: Holds<T>,
    {
        self.space.arena().is_occupied(gc.index())
    }

    /// Erase a handle's type.
    pub fn erase<T>(&self, gc: Gc<T>) -> AnyGc
    where
        S: Holds<T>,
    {
        AnyGc {
            kind: <S as Holds<T>>::KIND,
            index: gc.index(),
        }
    }

    /// Recover a typed handle, if `gc` points into `T`'s arena.
    pub fn downcast<T>(&self, gc: AnyGc) -> Option<Gc<T>>
    where
        S: Holds<T>,
    {
        (gc.kind == <S as Holds<T>>::KIND).then(|| Gc::from_index(gc.index))
    }

    // ---- barriers -------------------------------------------------------

    /// Call before storing a handle into `gc`. If the collector has already
    /// traced `gc` in this cycle, it is traced again in the atomic phase.
    /// Best for objects written often, such as tables.
    #[inline(always)]
    pub fn barrier_back<T>(&mut self, gc: Gc<T>)
    where
        S: Holds<T>,
    {
        if self.state == State::Mark {
            let kind = <S as Holds<T>>::KIND;
            let c = &mut self.marks.meta[kind as usize].color[gc.index() as usize];
            if *c & COLOR == BLACK {
                *c = (*c & !COLOR) | GRAY;
                self.gray_again.push(AnyGc {
                    kind,
                    index: gc.index(),
                });
            }
        }
    }

    /// Call when storing `child` into `owner`. If the collector has already
    /// traced `owner` in this cycle, `child` is marked now. Best for objects
    /// written rarely, such as closed upvalues.
    #[inline(always)]
    pub fn barrier_forward<T, U: Trace<S>>(&mut self, owner: Gc<T>, child: Gc<U>)
    where
        S: Holds<T> + Holds<U>,
    {
        if self.state == State::Mark
            && self.marks.meta[<S as Holds<T>>::KIND as usize].color[owner.index() as usize] & COLOR
                == BLACK
        {
            self.marks
                .mark_raw(<S as Holds<U>>::KIND, child.index(), U::LEAF);
        }
    }

    // ---- finalization ---------------------------------------------------

    /// Ask to be told when `gc` becomes unreachable (Lua's `__gc`). Returns
    /// false if it is already registered.
    ///
    /// When a cycle finds it unreachable, the object and everything it
    /// reaches are kept alive (resurrected) and it is queued for
    /// [`Heap::take_finalizable`]. It is registered once: after it has been
    /// taken, it is collected normally unless registered again.
    pub fn register_finalizer<T>(&mut self, gc: Gc<T>) -> bool
    where
        S: Holds<T>,
    {
        let kind = <S as Holds<T>>::KIND;
        let c = &mut self.marks.meta[kind as usize].color[gc.index() as usize];
        debug_assert!(*c != FREE, "orbit-gc: registering a stale handle");
        if *c & FIN != 0 {
            return false;
        }
        *c |= FIN;
        self.finobj.push(AnyGc {
            kind,
            index: gc.index(),
        });
        true
    }

    /// The next object whose finalizer should run, most recently registered
    /// first. The heap stops keeping it alive: root it (on the VM stack, say)
    /// before the next step, for as long as the finalizer runs.
    pub fn take_finalizable(&mut self) -> Option<AnyGc> {
        let gc = self.tobefnz.pop_front()?;
        self.marks.meta[gc.kind as usize].color[gc.index as usize] &= !FIN;
        Some(gc)
    }

    /// How many objects are waiting in [`Heap::take_finalizable`].
    pub fn pending_finalizers(&self) -> usize {
        self.tobefnz.len()
    }

    /// Queue every registered object for finalization, reachable or not, as
    /// when a Lua state closes.
    pub fn finalize_all(&mut self) {
        for gc in self.finobj.drain(..).rev() {
            self.tobefnz.push_back(gc);
        }
    }

    // ---- pacing ---------------------------------------------------------

    fn charge(&mut self, n: usize) {
        self.bytes += n;
        self.debt += n as isize;
    }

    /// Report that objects grew (positive) or shrank (negative) outside
    /// [`Heap::update`], so pacing and [`Heap::bytes`] stay right.
    pub fn adjust_bytes(&mut self, delta: isize) {
        if delta >= 0 {
            self.charge(delta as usize);
        } else {
            self.bytes = self.bytes.saturating_sub(delta.unsigned_abs());
        }
    }

    /// Whether the owner should call [`Heap::step`] at its next safe point.
    #[inline(always)]
    pub fn should_step(&self) -> bool {
        !self.stopped && (self.debt > 0 || self.stress)
    }

    /// Bytes in use: arena slots plus what objects report in
    /// [`Trace::extra_bytes`]. Lua's `collectgarbage("count")`.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Allocation debt: positive when a step is due.
    pub fn debt(&self) -> isize {
        self.debt
    }

    /// Live objects of type `T`, including unreachable ones not yet swept.
    pub fn count<T>(&self) -> usize
    where
        S: Holds<T>,
    {
        let meta = &self.marks.meta[<S as Holds<T>>::KIND as usize];
        meta.color.len() - meta.free.len()
    }

    /// Completed collection cycles.
    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    pub fn phase(&self) -> Phase {
        match self.state {
            State::Pause => Phase::Pause,
            State::Mark => Phase::Mark,
            State::Sweep { .. } => Phase::Sweep,
        }
    }

    pub fn params(&self) -> Params {
        self.params
    }

    pub fn params_mut(&mut self) -> &mut Params {
        &mut self.params
    }

    /// Stop automatic collection: [`Heap::should_step`] stays false. Explicit
    /// [`Heap::step`] and [`Heap::full_collect`] still work. Lua's
    /// `collectgarbage("stop")`.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Undo [`Heap::stop`].
    pub fn restart(&mut self) {
        self.stopped = false;
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Stress mode: a step is due on every check, each step does a little
    /// work, and a new cycle starts as soon as one ends. Slow, but it
    /// collects at nearly every safe point, which flushes out missing roots
    /// and barriers.
    pub fn set_stress(&mut self, on: bool) {
        self.stress = on;
        if !on {
            self.debt = self.bytes as isize - self.threshold as isize;
        }
    }

    pub fn is_stress(&self) -> bool {
        self.stress
    }

    // ---- driving --------------------------------------------------------

    /// Do one increment of collection work, proportional to the allocation
    /// debt. Returns true if this step finished a cycle.
    pub fn step<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) -> bool {
        let mut work = if self.stress {
            STRESS_WORK
        } else {
            let debt = self.debt.max(self.params.stepsize as isize) as usize;
            (debt.saturating_mul(self.params.stepmul as usize) / 100).max(1)
        };
        loop {
            match self.state {
                State::Pause => {
                    self.mark_roots(roots);
                    self.state = State::Mark;
                }
                State::Mark => {
                    while work > 0 {
                        // As in PUC Lua, objects regrayed by the barrier wait
                        // for the atomic phase: retracing a big, hot table
                        // after every store would be quadratic.
                        let Some(g) = self.marks.gray.pop() else {
                            break;
                        };
                        work = work.saturating_sub(self.blacken(g, true).max(1));
                    }
                    if !self.marks.gray.is_empty() {
                        break;
                    }
                    self.atomic(roots);
                    self.state = State::Sweep { kind: 0, at: 0 };
                }
                State::Sweep { .. } => {
                    if self.sweep(work / 32 + 1) {
                        self.finish_cycle();
                        return true;
                    }
                    break;
                }
            }
        }
        self.debt = if self.stress {
            0
        } else {
            -(self.params.stepsize as isize)
        };
        false
    }

    /// Finish any cycle in progress, then run a whole cycle. Lua's
    /// `collectgarbage("collect")`.
    pub fn full_collect<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) {
        match self.state {
            State::Pause => {}
            // Objects marked earlier in this cycle may have died since. As
            // Lua does, drop the partial marking and start over, so a full
            // collection frees everything unreachable now.
            State::Mark => self.unmark_all(),
            State::Sweep { .. } => while !self.sweep(usize::MAX) {},
        }
        self.mark_roots(roots);
        self.state = State::Mark;
        self.atomic(roots);
        self.state = State::Sweep { kind: 0, at: 0 };
        while !self.sweep(usize::MAX) {}
        self.finish_cycle();
    }

    fn unmark_all(&mut self) {
        for meta in &mut self.marks.meta {
            for c in &mut meta.color {
                if *c != FREE {
                    *c &= !COLOR;
                }
            }
        }
        self.marks.gray.clear();
        self.gray_again.clear();
        self.weak.clear();
        self.state = State::Pause;
    }

    fn finish_cycle(&mut self) {
        self.state = State::Pause;
        self.cycles += 1;
        self.threshold = (self.estimate / 100)
            .saturating_mul(self.params.pause as usize)
            .max(self.params.min_threshold);
        self.debt = if self.stress {
            0
        } else {
            self.bytes as isize - self.threshold as isize
        };
    }

    // ---- marking --------------------------------------------------------

    fn marker(&mut self, can_register: bool) -> Marker<'_, S> {
        Marker {
            space: &self.space,
            marks: &mut self.marks,
            weak: None,
            can_register,
        }
    }

    fn mark_roots<R: Roots<S> + ?Sized>(&mut self, roots: &R) {
        let mut m = self.marker(false);
        roots.trace(&mut m);
        for &gc in &self.tobefnz {
            self.marks.mark_raw(gc.kind, gc.index, false);
        }
    }

    /// Trace one gray object and blacken it. Returns the work done, in bytes.
    fn blacken(&mut self, g: AnyGc, can_register: bool) -> usize {
        let c = &mut self.marks.meta[g.kind as usize].color[g.index as usize];
        *c = (*c & !COLOR) | BLACK;
        let mut m = Marker {
            space: &self.space,
            marks: &mut self.marks,
            weak: None,
            can_register,
        };
        let work = self.space.trace_slot(g.kind, g.index, &mut m);
        if let Some(mode) = m.weak {
            self.weak.push((g, mode));
        }
        work
    }

    fn propagate_all(&mut self) {
        loop {
            if let Some(g) = self.marks.gray.pop() {
                self.blacken(g, true);
            } else if let Some(g) = self.gray_again.pop() {
                self.blacken(g, true);
            } else {
                break;
            }
        }
    }

    /// Mark until every ephemeron whose key is marked has its value marked.
    fn converge_ephemerons(&mut self) {
        loop {
            self.propagate_all();
            let before = self.marks.marked;
            for i in 0..self.weak.len() {
                let (g, mode) = self.weak[i];
                if mode.is_ephemeron() {
                    self.blacken(g, false);
                }
            }
            if self.marks.marked == before {
                break;
            }
        }
    }

    fn dedup_weak(&mut self) {
        // An object traced twice in a cycle (regrayed by a barrier) declares
        // itself twice; keep the latest mode.
        self.weak.reverse();
        self.weak.sort_by_key(|&(g, _)| g);
        self.weak.dedup_by_key(|&mut (g, _)| g);
    }

    fn clear_weak(&mut self, only_values: bool) {
        for i in 0..self.weak.len() {
            let (g, mut mode) = self.weak[i];
            if only_values {
                if !mode.values {
                    continue;
                }
                mode.keys = false;
            }
            let live = Liveness {
                marks: &self.marks,
                _s: PhantomData,
            };
            self.space.clear_slot(g.kind, g.index, mode, &live);
        }
    }

    /// The indivisible end of marking, in `lgc.c`'s order.
    fn atomic<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) {
        // Roots are rescanned, so writes to them need no barrier.
        self.mark_roots(roots);
        self.propagate_all();
        self.dedup_weak();
        self.converge_ephemerons();
        // Everything strongly reachable is marked. Clear weak values now, so
        // none of them exposes an object about to be finalized.
        self.clear_weak(true);
        // Resurrect unreachable finalizable objects, newest first.
        let mut dead = Vec::new();
        self.finobj.retain(|&g| {
            let alive = self.marks.is_marked(g.kind, g.index);
            if !alive {
                dead.push(g);
            }
            alive
        });
        for &g in dead.iter().rev() {
            self.tobefnz.push_back(g);
            self.marks.mark_raw(g.kind, g.index, false);
        }
        self.converge_ephemerons();
        self.dedup_weak();
        self.clear_weak(false);
        let live = Liveness {
            marks: &self.marks,
            _s: PhantomData,
        };
        roots.clear_weak(&live);
        self.weak.clear();
        self.estimate = self.bytes;
    }

    // ---- sweeping -------------------------------------------------------

    /// Sweep until `budget` objects have been visited. Free slots are cheap
    /// to skip, so each counts as 1/16 of an object; otherwise an arena that
    /// grew and then emptied would slow every later sweep down to a crawl.
    /// Returns true when every arena is swept.
    fn sweep(&mut self, budget: usize) -> bool {
        let mut credit = budget.saturating_mul(FREE_SLOT_SHARE);
        loop {
            let State::Sweep { kind, at } = self.state else {
                return true;
            };
            if kind as usize >= S::KINDS {
                return true;
            }
            if credit == 0 {
                return false;
            }
            let len = self.marks.meta[kind as usize].color.len();
            let mut i = at as usize;
            while i < len && credit > 0 {
                let cost = if self.sweep_one(kind, i as u32) {
                    FREE_SLOT_SHARE
                } else {
                    1
                };
                credit = credit.saturating_sub(cost);
                i += 1;
            }
            self.state = if i == len {
                State::Sweep {
                    kind: kind + 1,
                    at: 0,
                }
            } else {
                State::Sweep { kind, at: i as u32 }
            };
        }
    }

    /// Sweep one slot. Returns false if it was already free.
    fn sweep_one(&mut self, kind: u8, index: u32) -> bool {
        let meta = &mut self.marks.meta[kind as usize];
        let c = &mut meta.color[index as usize];
        if *c == FREE {
            return false;
        }
        if *c & COLOR == WHITE {
            debug_assert!(
                *c & FIN == 0,
                "orbit-gc: freeing an object awaiting finalization"
            );
            *c = FREE;
            meta.free.push(index);
            let n = self.space.free_slot(kind, index);
            self.bytes = self.bytes.saturating_sub(n);
            self.estimate = self.estimate.saturating_sub(n);
        } else {
            *c &= !COLOR;
        }
        true
    }
}
