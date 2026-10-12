//! The heap: allocation, barriers, pacing and the incremental collector.

use crate::arena::{AnyGc, Gc, Gen, Store, page_shift};
use crate::trace::{
    BLACK, COLOR, FIN, FREE, GRAY, Holds, KindMeta, Liveness, Marker, Marks, Roots, Space, Trace,
    WAIT, WHITE, WaitMap, WeakMode,
};
use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;

/// Collector tuning.
///
/// A cycle starts when the heap reaches [`Params::growth`] percent over the
/// bytes the last cycle found reachable, the way Go's `GOGC` works. While a
/// cycle runs, every [`Params::step_bytes`] of allocation buys a step that
/// processes [`Params::work_ratio`] percent of that many bytes of heap:
/// objects traced while marking, slots visited while sweeping. The higher
/// the ratio, the less the heap overshoots while a cycle is in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Params {
    /// How far the heap may grow past the bytes found reachable by the last
    /// cycle before the next one starts, in percent. 100 lets it double.
    /// 150 by default: on binary trees that keeps peak memory just under PUC
    /// Lua's while marking a third less often than 100 does.
    pub growth: u32,
    /// Bytes of heap processed per byte allocated while a cycle runs, in
    /// percent.
    pub work_ratio: u32,
    /// Bytes allocated between steps while a cycle runs. Smaller steps
    /// mean shorter pauses and more of them. 0 makes every step finish the
    /// cycle it is in: stop-the-world collection.
    pub step_bytes: usize,
    /// A floor under the heap size that starts a cycle, so small programs
    /// collect less often. 0 by default: with a floor, a small heap can't
    /// shrink back toward its live size.
    pub min_threshold: usize,
    /// With the `memory-trimming` feature: hand freed memory back to the OS
    /// once a cycle finds the reachable heap at or below this percentage of
    /// its peak since the last trim, and pages were released. 50 by default.
    /// Higher trims sooner after a smaller drop; trimming after every cycle
    /// costs time, because memory handed back is soon taken again.
    pub trim_percent: u32,
    /// With the `memory-trimming` feature: the least the reachable heap must
    /// have fallen from its peak, in bytes, before memory is handed back.
    /// 1 MiB by default, so small heaps are left alone.
    pub trim_min_bytes: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            growth: 150,
            work_ratio: 800,
            step_bytes: 16 * 1024,
            min_threshold: 0,
            trim_percent: 50,
            trim_min_bytes: 1 << 20,
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
/// Bytes of work done by one step in stress mode.
const STRESS_WORK: usize = 2048;

/// A garbage-collected heap over the arenas of space `S`.
///
/// Allocation never collects. The owner calls [`Heap::step`] at safe points
/// where every live handle is reachable from the roots it passes, typically
/// when [`Heap::should_step`] says a step is due.
pub struct Heap<S: Space> {
    space: S,
    marks: Marks,
    /// Objects a write barrier turned back to gray. They are traced again
    /// when marking finishes, not straight away, so a table written in a
    /// loop is retraced once rather than after every store.
    retrace: Vec<AnyGc>,
    state: State,
    /// Bytes allocated minus bytes freed.
    bytes: usize,
    /// Positive when a step is due.
    debt: isize,
    threshold: usize,
    params: Params,
    /// Objects that declared weak references in this cycle.
    weak: Vec<(AnyGc, WeakMode)>,
    /// Objects registered for finalization, oldest first.
    finalizable: Vec<AnyGc>,
    /// Unreachable objects whose finalizer is due, next first.
    finalize_queue: VecDeque<AnyGc>,
    cycles: u64,
    /// Arena pages released so far.
    pages_released: u64,
    /// `pages_released` when freed memory was last handed back to the OS.
    released_at_trim: u64,
    /// The most bytes a cycle has found reachable since memory was last
    /// handed back to the OS.
    live_peak: usize,
    stress: bool,
    stopped: bool,
    /// The most [`Heap::bytes`] the owner wants in use, or `usize::MAX`.
    limit: usize,
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
        let space = S::default();
        let meta = (0..S::KINDS)
            .map(|_| KindMeta::new(const { page_shift(S::PAGE_SLOTS) }))
            .collect();
        Heap {
            space,
            marks: Marks {
                meta,
                gray: Vec::new(),
                waiting: WaitMap::default(),
                scratch: Vec::new(),
                marked_bytes: 0,
            },
            retrace: Vec::new(),
            state: State::Pause,
            bytes: 0,
            debt: -(params.min_threshold as isize),
            threshold: params.min_threshold,
            params,
            weak: Vec::new(),
            finalizable: Vec::new(),
            finalize_queue: VecDeque::new(),
            cycles: 0,
            pages_released: 0,
            released_at_trim: 0,
            live_peak: 0,
            stress: false,
            stopped: false,
            limit: usize::MAX,
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
        let index = meta.take_slot();
        meta.color[index as usize] = alloc_color(state, kind, index);
        let gc = self.space.arena_mut().put(index, v);
        self.charge(size);
        gc
    }

    /// The object behind `gc`. Panics if it has been collected: always with
    /// [`CHECKED_HANDLES`](crate::CHECKED_HANDLES), otherwise only until its
    /// slot is reused.
    #[inline(always)]
    #[track_caller]
    pub fn get<T>(&self, gc: Gc<T>) -> &T
    where
        S: Holds<T>,
    {
        self.space.arena().get(gc)
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
        self.space.arena_mut().get_mut(gc)
    }

    /// Change an object: runs the backward write barrier, then `f`, then
    /// charges or credits any change in [`Trace::extra_bytes`].
    #[track_caller]
    pub fn update<T: Trace<S>, R>(&mut self, gc: Gc<T>, f: impl FnOnce(&mut T) -> R) -> R
    where
        S: Holds<T>,
    {
        self.barrier_back(gc);
        let obj = self.space.arena_mut().get_mut(gc);
        let before = obj.extra_bytes();
        let r = f(obj);
        let after = obj.extra_bytes();
        self.adjust_bytes(after as isize - before as isize);
        r
    }

    /// Whether `gc` still points at a live object. Without
    /// [`CHECKED_HANDLES`](crate::CHECKED_HANDLES) a reused slot also counts
    /// as live, so this is for tests and assertions.
    pub fn is_live<T>(&self, gc: Gc<T>) -> bool
    where
        S: Holds<T>,
    {
        self.space.arena().is_occupied(gc)
    }

    /// A handle to the object now in slot `index` of `T`'s arena, for code
    /// that stores bare indexes (an intern table, say) and knows the object
    /// there is still the one it means.
    #[track_caller]
    pub fn handle_at<T>(&self, index: u32) -> Gc<T>
    where
        S: Holds<T>,
    {
        self.space.arena().handle_at(index)
    }

    /// Erase a handle's type.
    pub fn erase<T>(&self, gc: Gc<T>) -> AnyGc
    where
        S: Holds<T>,
    {
        AnyGc {
            kind: <S as Holds<T>>::KIND,
            index: gc.index(),
            generation: Gen::new(gc.generation()),
        }
    }

    /// Recover a typed handle, if `gc` points into `T`'s arena.
    pub fn downcast<T>(&self, gc: AnyGc) -> Option<Gc<T>>
    where
        S: Holds<T>,
    {
        (gc.kind == <S as Holds<T>>::KIND)
            .then(|| Gc::from_bits(u64::from(gc.index) | u64::from(gc.generation.get()) << 32))
    }

    // ---- barriers -------------------------------------------------------

    /// Call before storing a handle into `gc`. If the collector has already
    /// traced `gc` in this cycle, it is traced again when marking finishes.
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
                self.retrace.push(AnyGc {
                    kind,
                    index: gc.index(),
                    generation: Gen::default(),
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
            let mut m = Marker {
                space: &self.space,
                marks: &mut self.marks,
                weak: None,
                can_register: false,
            };
            m.mark(child);
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
        self.finalizable.push(AnyGc {
            kind,
            index: gc.index(),
            generation: Gen::new(gc.generation()),
        });
        true
    }

    /// The next object whose finalizer should run, most recently registered
    /// first. The heap stops keeping it alive: root it (on the VM stack, say)
    /// before the next step, for as long as the finalizer runs.
    pub fn take_finalizable(&mut self) -> Option<AnyGc> {
        let gc = self.finalize_queue.pop_front()?;
        self.marks.meta[gc.kind as usize].color[gc.index as usize] &= !FIN;
        Some(gc)
    }

    /// How many objects are waiting in [`Heap::take_finalizable`].
    pub fn pending_finalizers(&self) -> usize {
        self.finalize_queue.len()
    }

    /// Queue every registered object for finalization, reachable or not, as
    /// when a Lua state closes.
    pub fn finalize_all(&mut self) {
        for gc in self.finalizable.drain(..).rev() {
            self.finalize_queue.push_back(gc);
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
        (self.debt > 0 || self.stress) && !self.stopped
    }

    /// Bytes in use: arena slots plus what objects report in
    /// [`Trace::extra_bytes`].
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
        self.marks.meta[<S as Holds<T>>::KIND as usize].live
    }

    /// Arena pages currently allocated, across all object types.
    pub fn pages(&self) -> usize {
        self.marks.meta.iter().map(|m| m.allocated).sum()
    }

    /// Bytes held by allocated pages: every slot, occupied or not, plus the
    /// collector's color byte per slot. Unlike [`Heap::bytes`], this is
    /// about what the arenas take from the allocator, and it drops when
    /// pages are released. A released page below a live one still holds its
    /// empty slots in its arena's vector until the pages above it go too
    /// (ADR-2); those are not counted.
    pub fn reserved_bytes(&self) -> usize {
        self.marks
            .meta
            .iter()
            .enumerate()
            .map(|(k, m)| m.allocated * m.page_slots() as usize * (S::slot_bytes(k as u8) + 1))
            .sum()
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

    /// Cap [`Heap::bytes`] at `limit` (`None`: no cap, the default). The
    /// heap never refuses an allocation itself: it reports. Once `bytes`
    /// passes the limit, [`Heap::should_step`] comes due at the next
    /// allocation and [`Heap::over_limit`] turns true; the owner then
    /// collects in full with its roots and, if the heap is still over,
    /// fails whatever was being done. Before a large allocation the owner
    /// asks [`Heap::would_exceed`]. Lowering the limit below `bytes` takes
    /// effect at the next allocation.
    pub fn set_limit(&mut self, limit: Option<usize>) {
        self.limit = limit.unwrap_or(usize::MAX);
        if self.threshold > self.limit {
            self.threshold = self.limit;
            self.debt = self.debt.max(self.bytes as isize - self.threshold as isize);
        }
    }

    /// The cap set by [`Heap::set_limit`].
    pub fn limit(&self) -> Option<usize> {
        (self.limit != usize::MAX).then_some(self.limit)
    }

    /// Whether [`Heap::bytes`] is past the limit.
    #[inline]
    pub fn over_limit(&self) -> bool {
        self.bytes > self.limit
    }

    /// Whether allocating `bytes` more would pass the limit.
    #[inline]
    pub fn would_exceed(&self, bytes: usize) -> bool {
        self.bytes.saturating_add(bytes) > self.limit
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

    /// Stress mode: a step is due on every check, each step after an
    /// allocation does a little work, and a new cycle starts as soon as one
    /// ends. Slow, but it collects at nearly every safe point that follows
    /// an allocation, which flushes out missing roots and barriers. A step
    /// with nothing allocated since the last one does nothing, as in normal
    /// mode, so a program that allocates nothing sees `bytes` stay put.
    pub fn set_stress(&mut self, on: bool) {
        self.stress = on;
        self.debt = if on {
            0
        } else {
            self.bytes as isize - self.threshold as isize
        };
    }

    pub fn is_stress(&self) -> bool {
        self.stress
    }

    // ---- driving --------------------------------------------------------

    /// Do one increment of collection work, proportional to the allocation
    /// debt. Returns true if this step finished a cycle.
    pub fn step<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) -> bool {
        let work = if self.stress {
            // `should_step` stays true in stress mode (testing the flag
            // there keeps the owner's hot check unchanged), so skip here.
            if self.debt <= 0 {
                return false;
            }
            STRESS_WORK
        } else {
            let debt = self.debt.max(self.params.step_bytes as isize) as usize;
            self.work_for(debt)
        };
        self.step_work(roots, work)
    }

    /// Do one increment of collection work as if `bytes` had been
    /// allocated, whatever the debt; 0 means one [`Params::step_bytes`]
    /// worth. Lua's `collectgarbage("step", n)`. Works while the collector
    /// is [stopped](Heap::stop), and doesn't restart it. Returns true if this
    /// step finished a cycle.
    pub fn step_with<R: Roots<S> + ?Sized>(&mut self, roots: &mut R, bytes: usize) -> bool {
        let bytes = if bytes == 0 {
            self.params.step_bytes
        } else {
            bytes
        };
        let work = self.work_for(bytes);
        self.step_work(roots, work)
    }

    /// Bytes of heap to process for `bytes` of allocation. With
    /// [`Params::step_bytes`] 0, a step finishes its cycle.
    fn work_for(&self, bytes: usize) -> usize {
        if self.params.step_bytes == 0 {
            return usize::MAX;
        }
        (bytes.saturating_mul(self.params.work_ratio as usize) / 100).max(1)
    }

    fn step_work<R: Roots<S> + ?Sized>(&mut self, roots: &mut R, mut work: usize) -> bool {
        loop {
            match self.state {
                State::Pause => self.start_marking(roots),
                State::Mark => {
                    // Objects regrayed by the backward barrier wait in
                    // `retrace` for the end of marking: retracing a big, hot
                    // table after every store would be quadratic.
                    while work > 0 {
                        let Some(g) = self.marks.gray.pop() else {
                            break;
                        };
                        let before = self.marks.marked_bytes;
                        self.blacken(g);
                        work = work.saturating_sub((self.marks.marked_bytes - before).max(1));
                    }
                    if !self.marks.gray.is_empty() {
                        break;
                    }
                    self.finish_marking(roots);
                }
                State::Sweep { .. } => {
                    if self.sweep(work) {
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
            -(self.params.step_bytes as isize)
        };
        false
    }

    /// Finish any cycle in progress, then run a whole cycle. Lua's
    /// `collectgarbage("collect")`.
    pub fn full_collect<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) {
        match self.state {
            State::Pause => {}
            // Objects marked earlier in this cycle may have died since. Drop
            // the partial marking and start over, so a full collection frees
            // everything unreachable now.
            State::Mark => self.unmark_all(),
            State::Sweep { .. } => while !self.sweep(usize::MAX) {},
        }
        self.start_marking(roots);
        self.finish_marking(roots);
        while !self.sweep(usize::MAX) {}
        self.finish_cycle();
    }

    fn unmark_all(&mut self) {
        for meta in &mut self.marks.meta {
            for c in &mut meta.color {
                if *c != FREE {
                    *c &= !(COLOR | WAIT);
                }
            }
        }
        self.marks.gray.clear();
        self.marks.waiting.clear();
        self.retrace.clear();
        self.weak.clear();
        self.state = State::Pause;
    }

    /// The next cycle starts once the heap grows `growth` percent past what
    /// this one found reachable.
    fn finish_cycle(&mut self) {
        self.state = State::Pause;
        self.cycles += 1;
        let live = self.marks.marked_bytes;
        self.release_to_os(live);
        // Never let the heap grow past the limit unchecked: the next cycle
        // starts there at the latest, which is also when the owner sees
        // `over_limit`.
        self.threshold = live
            .saturating_add(live.saturating_mul(self.params.growth as usize) / 100)
            .max(self.params.min_threshold)
            .min(self.limit);
        // If the heap is already past the next threshold, start the next
        // cycle at once, but in ordinary steps: carrying the whole overshoot
        // as debt would make its first step a long pause.
        self.debt = if self.stress {
            0
        } else {
            (self.bytes as isize - self.threshold as isize).min(0)
        };
    }

    /// Hand freed memory back to the OS when the heap has shrunk: pages
    /// were released, and the reachable heap is at most `trim_percent` of
    /// its peak, by at least `trim_min_bytes`. Trimming on every cycle that
    /// released a page instead cost 23% on binary trees, because the memory
    /// is taken straight back; under steady churn the live size holds, so
    /// this never trims.
    fn release_to_os(&mut self, live: usize) {
        if !crate::os::RELEASES {
            return;
        }
        self.live_peak = self.live_peak.max(live);
        if self.pages_released != self.released_at_trim
            && live.saturating_mul(100)
                <= self
                    .live_peak
                    .saturating_mul(self.params.trim_percent as usize)
            && self.live_peak - live >= self.params.trim_min_bytes
        {
            crate::os::release_memory();
            self.released_at_trim = self.pages_released;
            self.live_peak = live;
        }
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

    fn start_marking<R: Roots<S> + ?Sized>(&mut self, roots: &R) {
        self.marks.marked_bytes = 0;
        self.mark_roots(roots);
        self.state = State::Mark;
    }

    fn mark_roots<R: Roots<S> + ?Sized>(&mut self, roots: &R) {
        let mut m = self.marker(false);
        roots.trace(&mut m);
        for &gc in &self.finalize_queue {
            self.marks.mark_raw(gc.kind, gc.index, false);
        }
    }

    /// Trace one gray object, blacken it, and count its bytes as marked.
    fn blacken(&mut self, g: AnyGc) {
        let c = &mut self.marks.meta[g.kind as usize].color[g.index as usize];
        *c = (*c & !COLOR) | BLACK;
        let mut m = Marker {
            space: &self.space,
            marks: &mut self.marks,
            weak: None,
            can_register: true,
        };
        let size = self.space.trace_slot(g.kind, g.index, &mut m);
        if let Some(mode) = m.weak {
            self.weak.push((g, mode));
        }
        self.marks.marked_bytes += size;
    }

    /// Trace until nothing is gray, including objects waiting for a retrace.
    fn drain(&mut self) {
        while let Some(g) = self.marks.gray.pop().or_else(|| self.retrace.pop()) {
            self.blacken(g);
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

    /// Marking's last step, done in one go: after it, the reachable set is
    /// known and weak references are cleared. Then the sweep starts.
    ///
    /// The order follows Lua's documented rules. Weak values are cleared
    /// before finalizable objects are resurrected, so a weak value never
    /// exposes an object about to be finalized. Weak keys are cleared after,
    /// so an entry whose key is being finalized survives until the next
    /// cycle, and resurrected keys keep their ephemeron values.
    fn finish_marking<R: Roots<S> + ?Sized>(&mut self, roots: &mut R) {
        // Roots are rescanned, so writes to them need no barrier.
        self.mark_roots(roots);
        self.drain();
        self.dedup_weak();
        self.clear_weak(true);
        // Keep unreachable finalizable objects alive for their finalizers,
        // and queue them newest first.
        let mut dead = Vec::new();
        self.finalizable.retain(|&g| {
            let alive = self.marks.is_marked(g.kind, g.index);
            if !alive {
                dead.push(g);
            }
            alive
        });
        for &g in dead.iter().rev() {
            self.finalize_queue.push_back(g);
            self.marks.mark_raw(g.kind, g.index, false);
        }
        self.drain();
        // Objects traced for the first time just now registered too.
        self.dedup_weak();
        self.clear_weak(false);
        let live = Liveness {
            marks: &self.marks,
            _s: PhantomData,
        };
        roots.clear_weak(&live);
        self.weak.clear();
        self.marks.clear_waiting();
        self.state = State::Sweep { kind: 0, at: 0 };
    }

    // ---- sweeping -------------------------------------------------------

    /// Sweep about `budget` bytes of slots. A free slot is cheap to skip, so
    /// it counts as 1/16 of its size; otherwise an arena that grew and then
    /// emptied would slow every later sweep down to a crawl. Released pages
    /// and never-used slots cost nothing. A page left with no objects is
    /// released, unless it is its arena's last. Returns true when every arena
    /// is swept.
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
            let meta = &self.marks.meta[kind as usize];
            let (p, start) = meta.split(at);
            let Some(page) = meta.pages.get(p) else {
                self.state = State::Sweep {
                    kind: kind + 1,
                    at: 0,
                };
                continue;
            };
            let shift = meta.shift;
            let end = page.fresh as usize;
            // In 16ths of a byte, like `credit`.
            let slot_cost = (S::slot_bytes(kind) + 1) * FREE_SLOT_SHARE;
            let base = (p as u32) << shift;
            let mut o = start;
            while o < end && credit > 0 {
                let cost = if self.sweep_one(kind, base | o as u32) {
                    slot_cost
                } else {
                    slot_cost / FREE_SLOT_SHARE
                };
                credit = credit.saturating_sub(cost);
                o += 1;
            }
            self.state = if o < end {
                State::Sweep {
                    kind,
                    at: ((p as u32) << shift) | o as u32,
                }
            } else {
                self.finish_page(kind, p as u32);
                match ((p as u32) + 1).checked_mul(1 << shift) {
                    Some(at) => State::Sweep { kind, at },
                    None => State::Sweep {
                        kind: kind + 1,
                        at: 0,
                    },
                }
            };
        }
    }

    /// Release page `p` of `kind` if the sweep left it empty and it is not
    /// the arena's last page.
    fn finish_page(&mut self, kind: u8, p: u32) {
        let meta = &mut self.marks.meta[kind as usize];
        let page = &meta.pages[p as usize];
        if page.allocated && page.live == 0 && meta.allocated > 1 {
            meta.release_page(p);
            self.space.release_page(kind, p);
            self.pages_released += 1;
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
            meta.free_slot(index);
            let n = self.space.free_slot(kind, index);
            self.bytes = self.bytes.saturating_sub(n);
        } else {
            *c &= !COLOR;
        }
        true
    }
}
