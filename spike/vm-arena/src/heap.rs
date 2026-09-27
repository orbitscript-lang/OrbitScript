//! Safe typed arenas with `u32` handles and an incremental tri-color
//! mark-sweep collector, written as ordinary Rust.
//!
//! - One paged arena per object kind; colors live in a side byte vector.
//! - New objects are white, as in PUC Lua. Stores into black tables regray the
//!   table (backward barrier); stores into black closed upvalues mark the
//!   value (forward barrier). The VM stack and open upvalues are rescanned in
//!   the atomic phase, so register writes need no barrier.
//! - Weak-key tables (`__mode = "k"`) are ephemerons, converged in the atomic
//!   phase. Tables whose metatable has `__gc` are resurrected once and queued
//!   for finalization, in PUC order (keys are cleared after resurrection).

use crate::value::{FnRef, Key, StrRef, TabRef, UvRef, Value};
use spike_common::{FxBuild, str_hash};
use std::collections::HashMap;

const PAGE_BITS: u32 = 10;
const PAGE: usize = 1 << PAGE_BITS;

pub const WHITE: u8 = 0;
pub const GRAY: u8 = 1;
pub const BLACK: u8 = 2;

/// A paged arena. Pages never move, so growth never copies live objects and
/// never doubles peak memory.
#[derive(Debug)]
pub struct Arena<T> {
    pages: Vec<Vec<Option<T>>>,
    pub color: Vec<u8>,
    free: Vec<u32>,
    pub len: u32,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena {
            pages: Vec::new(),
            color: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }
}

impl<T> Arena<T> {
    fn insert(&mut self, v: T, color: impl Fn(u32) -> u8) -> u32 {
        if let Some(i) = self.free.pop() {
            self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)] = Some(v);
            self.color[i as usize] = color(i);
            return i;
        }
        let i = self.len;
        if (i as usize).is_multiple_of(PAGE) {
            let mut p = Vec::with_capacity(PAGE);
            p.resize_with(PAGE, || None);
            self.pages.push(p);
        }
        self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)] = Some(v);
        self.color.push(color(i));
        self.len += 1;
        i
    }
    #[inline(always)]
    pub fn get(&self, i: u32) -> &T {
        self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)]
            .as_ref()
            .expect("stale handle")
    }
    #[inline(always)]
    pub fn get_mut(&mut self, i: u32) -> &mut T {
        self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)]
            .as_mut()
            .expect("stale handle")
    }
    fn take(&mut self, i: u32) -> Option<T> {
        self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)].take()
    }
    fn is_live(&self, i: u32) -> bool {
        self.pages[(i >> PAGE_BITS) as usize][i as usize & (PAGE - 1)].is_some()
    }
    pub fn count(&self) -> usize {
        self.len as usize - self.free.len()
    }
}

#[derive(Debug)]
pub struct Str {
    pub hash: u64,
    pub bytes: Box<[u8]>,
}

#[derive(Debug, Default)]
pub struct Table {
    pub arr: Vec<Value>,
    pub hash: HashMap<Key, Value, FxBuild>,
    pub meta: Option<TabRef>,
}

#[derive(Debug)]
pub struct Closure<P> {
    pub proto: P,
    pub upvals: Box<[UvRef]>,
}

#[derive(Debug, Clone, Copy)]
pub enum Upval {
    Open(u32),
    Closed(Value),
}

#[derive(Debug, Clone, Copy)]
enum Gray {
    Tab(u32),
    Fn(u32),
    Uv(u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Pause,
    Mark,
    /// Sweeping arena `kind` (0 strs, 1 tabs, 2 fns, 3 uvs) at slot `at`.
    Sweep {
        kind: u8,
        at: u32,
    },
}

/// Short strings (<= 40 bytes, as in PUC Lua) are interned, so a string
/// handle is its identity. Long strings are interned lazily when used as keys.
#[derive(Debug, Default)]
struct Intern {
    slots: Vec<u32>,
    used: usize,
}
const EMPTY: u32 = u32::MAX;
const TOMB: u32 = u32::MAX - 1;
pub const SHORT_STR: usize = 40;

/// Roots the VM hands to the collector.
pub struct Roots<'a> {
    pub stack: &'a [Value],
    pub open: &'a [(u32, UvRef)],
    pub extra: &'a [Value],
}

#[derive(Debug)]
pub struct Heap<P> {
    pub strs: Arena<Str>,
    pub tabs: Arena<Table>,
    pub fns: Arena<Closure<P>>,
    pub uvs: Arena<Upval>,
    intern: Intern,
    gray: Vec<Gray>,
    gray_again: Vec<Gray>,
    pub phase: Phase,
    /// Bytes allocated minus bytes freed.
    pub bytes: usize,
    /// Positive when a step is due.
    pub debt: isize,
    threshold: usize,
    ephemerons: Vec<u32>,
    finobj: Vec<u32>,
    pub tobefnz: Vec<TabRef>,
    s_mode: StrRef,
    s_gc: StrRef,
    pub cycles: usize,
    /// GC stress mode: a small step on every allocation, no pause.
    pub stress: bool,
}

/// Collector tuning, matching PUC Lua 5.5 defaults (pause 250%, stepmul 200%).
const PAUSE: usize = 250;
const STEPMUL: isize = 200;
const STEPSIZE: isize = 200 * 56;

impl<P> Default for Heap<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P> Heap<P> {
    pub fn new() -> Self {
        let mut h = Heap {
            strs: Arena::default(),
            tabs: Arena::default(),
            fns: Arena::default(),
            uvs: Arena::default(),
            intern: Intern::default(),
            gray: Vec::new(),
            gray_again: Vec::new(),
            phase: Phase::Pause,
            bytes: 0,
            debt: 0,
            threshold: 256 * 1024,
            ephemerons: Vec::new(),
            finobj: Vec::new(),
            tobefnz: Vec::new(),
            s_mode: StrRef(0),
            s_gc: StrRef(0),
            cycles: 0,
            stress: false,
        };
        h.intern.slots = vec![EMPTY; 1024];
        h.s_mode = h.new_str(b"__mode");
        h.s_gc = h.new_str(b"__gc");
        h.debt = -(h.threshold as isize);
        h
    }

    /// Collect in tiny steps on every allocation, with no pause between cycles.
    pub fn set_stress(&mut self) {
        self.stress = true;
        self.debt = 0;
    }

    // ---- allocation -----------------------------------------------------

    fn alloc_color(&self, kind: u8) -> impl Fn(u32) -> u8 + use<P> {
        let phase = self.phase;
        move |i| match phase {
            Phase::Sweep { kind: k, at } if kind > k || (kind == k && i >= at) => BLACK,
            _ => WHITE,
        }
    }

    fn charge(&mut self, n: usize) {
        self.bytes += n;
        self.debt += n as isize;
    }

    pub fn new_str(&mut self, b: &[u8]) -> StrRef {
        let hash = str_hash(b);
        if b.len() <= SHORT_STR
            && let Some(s) = self.intern_find(b, hash)
        {
            // A dead string not yet swept may be revived: keep it alive.
            // (During marking the VM stack rescan marks it.)
            if let Phase::Sweep { kind: 0, at } = self.phase
                && s >= at
            {
                self.strs.color[s as usize] = BLACK;
            }
            return StrRef(s);
        }
        let c = self.alloc_color(0);
        let s = self.strs.insert(
            Str {
                hash,
                bytes: b.into(),
            },
            c,
        );
        self.charge(24 + 16 + b.len());
        if b.len() <= SHORT_STR {
            self.intern_insert(s, hash);
        }
        StrRef(s)
    }

    pub fn new_table(&mut self) -> TabRef {
        let c = self.alloc_color(1);
        let t = self.tabs.insert(Table::default(), c);
        self.charge(size_of::<Table>() + 8);
        TabRef(t)
    }

    pub fn new_closure(&mut self, proto: P, upvals: Box<[UvRef]>) -> FnRef {
        let n = upvals.len();
        let c = self.alloc_color(2);
        let f = self.fns.insert(Closure { proto, upvals }, c);
        self.charge(size_of::<Closure<P>>() + 16 + 4 * n);
        FnRef(f)
    }

    pub fn new_upval(&mut self, u: Upval) -> UvRef {
        let c = self.alloc_color(3);
        let r = self.uvs.insert(u, c);
        self.charge(size_of::<Upval>() + 8);
        UvRef(r)
    }

    fn intern_find(&self, b: &[u8], hash: u64) -> Option<u32> {
        let mask = self.intern.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            match self.intern.slots[i] {
                EMPTY => return None,
                TOMB => {}
                s => {
                    let st = self.strs.get(s);
                    if st.hash == hash && &*st.bytes == b {
                        return Some(s);
                    }
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn intern_insert(&mut self, s: u32, hash: u64) {
        if (self.intern.used + 1) * 2 > self.intern.slots.len() {
            let live: Vec<u32> = self
                .intern
                .slots
                .iter()
                .copied()
                .filter(|&x| x != EMPTY && x != TOMB)
                .collect();
            let mut cap = self.intern.slots.len();
            while live.len() * 4 > cap {
                cap *= 2;
            }
            self.intern.slots = vec![EMPTY; cap];
            self.intern.used = 0;
            for x in live {
                let h = self.strs.get(x).hash;
                self.intern_put(x, h);
            }
        }
        self.intern_put(s, hash);
    }

    fn intern_put(&mut self, s: u32, hash: u64) {
        let mask = self.intern.slots.len() - 1;
        let mut i = hash as usize & mask;
        while self.intern.slots[i] != EMPTY {
            i = (i + 1) & mask;
        }
        self.intern.slots[i] = s;
        self.intern.used += 1;
    }

    fn intern_remove(&mut self, s: u32, hash: u64) {
        let mask = self.intern.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            match self.intern.slots[i] {
                EMPTY => return,
                x if x == s => {
                    self.intern.slots[i] = TOMB;
                    return;
                }
                _ => i = (i + 1) & mask,
            }
        }
    }

    /// Canonical string handle for use as a table key.
    #[inline]
    pub fn key_str(&mut self, s: StrRef) -> StrRef {
        let st = self.strs.get(s.0);
        if st.bytes.len() <= SHORT_STR {
            return s;
        }
        let (hash, bytes) = (st.hash, st.bytes.clone());
        match self.intern_find(&bytes, hash) {
            Some(x) => StrRef(x),
            None => {
                self.intern_insert(s.0, hash);
                s
            }
        }
    }

    #[inline]
    pub fn key(&mut self, v: Value) -> Option<Key> {
        match v {
            Value::Str(s) => Some(Key::Str(self.key_str(s).0)),
            _ => Key::from_value(v),
        }
    }

    // ---- tables ---------------------------------------------------------

    pub fn get(&self, t: TabRef, k: Key) -> Value {
        let tab = self.tabs.get(t.0);
        if let Key::Int(i) = k
            && (i as u64).wrapping_sub(1) < tab.arr.len() as u64
        {
            return tab.arr[(i - 1) as usize];
        }
        tab.hash.get(&k).copied().unwrap_or(Value::Nil)
    }

    #[inline(always)]
    pub fn get_int(&self, t: TabRef, i: i64) -> Value {
        let tab = self.tabs.get(t.0);
        if (i as u64).wrapping_sub(1) < tab.arr.len() as u64 {
            return tab.arr[(i - 1) as usize];
        }
        tab.hash.get(&Key::Int(i)).copied().unwrap_or(Value::Nil)
    }

    pub fn set(&mut self, t: TabRef, k: Key, v: Value) {
        self.barrier_back(t);
        let tab = self.tabs.get_mut(t.0);
        let before = tab.arr.capacity() * 16 + tab.hash.capacity() * 40;
        if let Key::Int(i) = k {
            let n = tab.arr.len() as i64;
            if i >= 1 && i <= n {
                tab.arr[(i - 1) as usize] = v;
                return;
            }
            if i == n + 1 && !v.is_nil() {
                tab.arr.push(v);
                // Migrate following keys out of the hash part.
                if !tab.hash.is_empty() {
                    let mut j = n + 2;
                    while let Some(x) = tab.hash.remove(&Key::Int(j)) {
                        tab.arr.push(x);
                        j += 1;
                    }
                }
                let after = tab.arr.capacity() * 16 + tab.hash.capacity() * 40;
                if after > before {
                    self.charge(after - before);
                }
                return;
            }
        }
        if v.is_nil() {
            tab.hash.remove(&k);
        } else {
            tab.hash.insert(k, v);
        }
        let after = tab.arr.capacity() * 16 + tab.hash.capacity() * 40;
        if after > before {
            self.charge(after - before);
        }
    }

    pub fn len(&self, t: TabRef) -> i64 {
        let tab = self.tabs.get(t.0);
        let mut n = tab.arr.len() as i64;
        while tab.hash.contains_key(&Key::Int(n + 1)) {
            n += 1;
        }
        n
    }

    pub fn set_metatable(&mut self, t: TabRef, mt: TabRef) {
        self.barrier_back(t);
        self.tabs.get_mut(t.0).meta = Some(mt);
        if !self.get(mt, Key::Str(self.s_gc.0)).is_nil() {
            self.finobj.push(t.0);
        }
    }

    // ---- barriers -------------------------------------------------------

    #[inline(always)]
    pub fn barrier_back(&mut self, t: TabRef) {
        if self.tabs.color[t.0 as usize] == BLACK && self.phase == Phase::Mark {
            self.tabs.color[t.0 as usize] = GRAY;
            self.gray_again.push(Gray::Tab(t.0));
        }
    }

    #[inline(always)]
    pub fn barrier_fwd_uv(&mut self, u: UvRef, v: Value) {
        if self.phase == Phase::Mark && self.uvs.color[u.0 as usize] == BLACK {
            self.mark(v);
        }
    }

    // ---- marking --------------------------------------------------------

    #[inline]
    fn mark(&mut self, v: Value) {
        match v {
            Value::Str(s) => self.strs.color[s.0 as usize] = BLACK,
            Value::Table(t) => self.mark_tab(t.0),
            Value::Func(f) => self.mark_fn(f.0),
            _ => {}
        }
    }
    fn mark_tab(&mut self, t: u32) {
        if self.tabs.color[t as usize] == WHITE {
            self.tabs.color[t as usize] = GRAY;
            self.gray.push(Gray::Tab(t));
        }
    }
    fn mark_fn(&mut self, f: u32) {
        if self.fns.color[f as usize] == WHITE {
            self.fns.color[f as usize] = GRAY;
            self.gray.push(Gray::Fn(f));
        }
    }
    fn mark_uv(&mut self, u: u32) {
        if self.uvs.color[u as usize] == WHITE {
            self.uvs.color[u as usize] = GRAY;
            self.gray.push(Gray::Uv(u));
        }
    }
    fn mark_key(&mut self, k: Key) {
        match k {
            Key::Str(s) => self.strs.color[s as usize] = BLACK,
            Key::Table(t) => self.mark_tab(t),
            Key::Func(f) => self.mark_fn(f),
            _ => {}
        }
    }
    fn key_marked(&self, k: Key) -> bool {
        match k {
            Key::Table(t) => self.tabs.color[t as usize] != WHITE,
            Key::Func(f) => self.fns.color[f as usize] != WHITE,
            _ => true, // strings and numbers are values, never weak
        }
    }
    fn is_weak_keys(&self, t: u32) -> bool {
        match self.tabs.get(t).meta {
            Some(mt) => match self.get(mt, Key::Str(self.s_mode.0)) {
                Value::Str(s) => self.strs.get(s.0).bytes.contains(&b'k'),
                _ => false,
            },
            None => false,
        }
    }

    /// Blacken one gray object; returns the work done in bytes.
    fn blacken(&mut self, g: Gray) -> usize {
        match g {
            Gray::Tab(t) => {
                self.tabs.color[t as usize] = BLACK;
                if let Some(mt) = self.tabs.get(t).meta {
                    self.mark_tab(mt.0);
                }
                let weak = self.tabs.get(t).meta.is_some() && self.is_weak_keys(t);
                // Temporarily move the table out so we can mark while reading it.
                let tab = std::mem::take(self.tabs.get_mut(t));
                for &v in &tab.arr {
                    self.mark(v);
                }
                let mut pending = false;
                for (&k, &v) in &tab.hash {
                    if weak && !self.key_marked(k) {
                        pending = true;
                    } else {
                        self.mark_key(k);
                        self.mark(v);
                    }
                }
                let work = 64 + tab.arr.len() * 16 + tab.hash.len() * 40;
                *self.tabs.get_mut(t) = tab;
                if weak && (pending || !self.ephemerons.contains(&t)) {
                    self.ephemerons.push(t);
                }
                work
            }
            Gray::Fn(f) => {
                self.fns.color[f as usize] = BLACK;
                let n = self.fns.get(f).upvals.len();
                for i in 0..n {
                    let u = self.fns.get(f).upvals[i];
                    self.mark_uv(u.0);
                }
                32 + n * 4
            }
            Gray::Uv(u) => {
                self.uvs.color[u as usize] = BLACK;
                if let Upval::Closed(v) = *self.uvs.get(u) {
                    self.mark(v);
                }
                16
            }
        }
    }

    fn propagate_all(&mut self) {
        loop {
            if let Some(g) = self.gray.pop() {
                self.blacken(g);
            } else if let Some(g) = self.gray_again.pop() {
                self.blacken(g);
            } else {
                break;
            }
        }
    }

    fn mark_roots(&mut self, r: &Roots) {
        for &v in r.stack.iter().chain(r.extra) {
            self.mark(v);
        }
        for &(_, u) in r.open {
            self.mark_uv(u.0);
        }
        self.strs.color[self.s_mode.0 as usize] = BLACK;
        self.strs.color[self.s_gc.0 as usize] = BLACK;
        for t in self.tobefnz.clone() {
            self.mark_tab(t.0);
        }
    }

    fn converge_ephemerons(&mut self) {
        loop {
            self.propagate_all();
            let mut changed = false;
            for i in 0..self.ephemerons.len() {
                let t = self.ephemerons[i];
                if self.tabs.color[t as usize] == WHITE {
                    continue;
                }
                let tab = std::mem::take(self.tabs.get_mut(t));
                for (&k, &v) in &tab.hash {
                    if self.key_marked(k) {
                        let before = self.gray.len();
                        self.mark_key(k);
                        self.mark(v);
                        changed |= self.gray.len() != before;
                    }
                }
                *self.tabs.get_mut(t) = tab;
            }
            if !changed {
                break;
            }
        }
    }

    fn atomic(&mut self, r: &Roots) {
        self.mark_roots(r);
        self.converge_ephemerons();
        // Resurrect unreachable finalizable objects, then mark from them.
        let mut i = 0;
        while i < self.finobj.len() {
            let t = self.finobj[i];
            if self.tabs.color[t as usize] == WHITE {
                self.finobj.swap_remove(i);
                self.tobefnz.push(TabRef(t));
                self.mark_tab(t);
            } else {
                i += 1;
            }
        }
        self.converge_ephemerons();
        // Clear ephemeron entries whose keys are dead.
        for t in std::mem::take(&mut self.ephemerons) {
            if self.tabs.color[t as usize] == WHITE {
                continue;
            }
            let mut tab = std::mem::take(self.tabs.get_mut(t));
            tab.hash.retain(|&k, _| self.key_marked(k));
            *self.tabs.get_mut(t) = tab;
        }
    }

    // ---- sweeping -------------------------------------------------------

    /// Sweep up to `budget` slots; returns true when the whole heap is swept.
    fn sweep(&mut self, mut budget: usize) -> bool {
        while budget > 0 {
            let Phase::Sweep { kind, at } = self.phase else {
                return true;
            };
            let len = match kind {
                0 => self.strs.len,
                1 => self.tabs.len,
                2 => self.fns.len,
                3 => self.uvs.len,
                _ => return true,
            };
            let end = (at as usize + budget).min(len as usize) as u32;
            for i in at..end {
                self.sweep_one(kind, i);
            }
            budget -= (end - at) as usize;
            self.phase = if end == len {
                Phase::Sweep {
                    kind: kind + 1,
                    at: 0,
                }
            } else {
                Phase::Sweep { kind, at: end }
            };
            if kind + 1 > 3 && end == len {
                return true;
            }
        }
        false
    }

    fn sweep_one(&mut self, kind: u8, i: u32) {
        macro_rules! sweep {
            ($arena:expr, $on_free:expr) => {{
                let a = &mut $arena;
                if a.is_live(i) {
                    if a.color[i as usize] == WHITE {
                        let v = a.take(i).expect("live");
                        a.free.push(i);
                        #[allow(clippy::redundant_closure_call)]
                        let n = ($on_free)(self, v);
                        self.bytes = self.bytes.saturating_sub(n);
                    } else {
                        a.color[i as usize] = WHITE;
                    }
                }
            }};
        }
        match kind {
            0 => sweep!(self.strs, |h: &mut Self, s: Str| {
                h.intern_remove(i, s.hash);
                40 + s.bytes.len()
            }),
            1 => sweep!(self.tabs, |_h: &mut Self, t: Table| size_of::<Table>()
                + 8
                + t.arr.capacity() * 16
                + t.hash.capacity() * 40),
            2 => sweep!(
                self.fns,
                |_h: &mut Self, f: Closure<P>| size_of::<Closure<P>>() + 16 + 4 * f.upvals.len()
            ),
            _ => sweep!(self.uvs, |_h: &mut Self, _u: Upval| size_of::<Upval>() + 8),
        }
    }

    // ---- driving --------------------------------------------------------

    /// One incremental step, run when `debt > 0`.
    pub fn step(&mut self, r: &Roots) {
        let mut work = if self.stress {
            256
        } else {
            (self.debt.max(STEPSIZE) * STEPMUL / 100) as usize
        };
        loop {
            match self.phase {
                Phase::Pause => {
                    if self.bytes < self.threshold && !self.stress {
                        self.debt = self.bytes as isize - self.threshold as isize;
                        return;
                    }
                    self.mark_roots(r);
                    self.phase = Phase::Mark;
                }
                Phase::Mark => {
                    while work > 0 {
                        // As in PUC Lua, regrayed objects wait for the atomic
                        // phase; re-traversing a big hot table incrementally
                        // after every store is quadratic.
                        let Some(g) = self.gray.pop() else { break };
                        work = work.saturating_sub(self.blacken(g));
                    }
                    if work == 0 {
                        break;
                    }
                    self.atomic(r);
                    self.phase = Phase::Sweep { kind: 0, at: 0 };
                }
                Phase::Sweep { .. } => {
                    let slots = work / 32 + 1;
                    if self.sweep(slots) {
                        self.phase = Phase::Pause;
                        self.cycles += 1;
                        self.threshold = (self.bytes / 100 * PAUSE).max(256 * 1024);
                        self.debt = if self.stress {
                            0
                        } else {
                            self.bytes as isize - self.threshold as isize
                        };
                        return;
                    }
                    break;
                }
            }
        }
        self.debt = if self.stress { 0 } else { -STEPSIZE };
    }

    /// Run a full collection cycle (finishing any cycle in progress).
    pub fn full_gc(&mut self, r: &Roots) {
        if let Phase::Sweep { .. } = self.phase {
            while !self.sweep(usize::MAX) {}
            self.phase = Phase::Pause;
        }
        if self.phase == Phase::Pause {
            self.mark_roots(r);
            self.phase = Phase::Mark;
        }
        self.propagate_all();
        self.atomic(r);
        self.phase = Phase::Sweep { kind: 0, at: 0 };
        while !self.sweep(usize::MAX) {}
        self.phase = Phase::Pause;
        self.cycles += 1;
        self.threshold = (self.bytes / 100 * PAUSE).max(256 * 1024);
        self.debt = self.bytes as isize - self.threshold as isize;
    }

    pub fn str_bytes(&self, s: StrRef) -> &[u8] {
        &self.strs.get(s.0).bytes
    }

    pub fn tab_is_live(&self, t: TabRef) -> bool {
        self.tabs.is_live(t.0)
    }
}
