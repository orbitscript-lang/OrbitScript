//! Micro-VM on gc-arena 0.7 (ADR-2 option A).
//!
//! Same bytecode and semantics as `spike-vm-arena`. The interpreter runs in
//! slices inside `Arena::mutate_root`, returning to the host whenever the
//! allocation debt is positive so the collector can run.
//!
//! Collection is driven by hand (`mark_debt` → `finalize` → `start_sweeping`)
//! so that every cycle passes through the fully-marked state, where
//! ephemerons are converged and `__gc` objects are resurrected.

use gc_arena::{
    Arena, Collect, Finalization, Gc, GcWeak, Lock, Mutation, RefLock, Rootable,
    arena::CollectionPhase,
};
use spike_common::{Const, FxBuild, Op, Proto, UpvalDesc, fuse, lua_imod, str_hash};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Collect, Debug)]
#[collect(require_static)]
pub struct Str {
    pub hash: u64,
    pub bytes: Box<[u8]>,
}

pub type Tab<'gc> = Gc<'gc, RefLock<Table<'gc>>>;
pub type Func<'gc> = Gc<'gc, Closure<'gc>>;
pub type Upval<'gc> = Gc<'gc, Lock<UpvalState<'gc>>>;

#[derive(Collect, Clone, Copy, Debug)]
#[collect(no_drop)]
pub enum Value<'gc> {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Gc<'gc, Str>),
    Table(Tab<'gc>),
    Func(Func<'gc>),
}

impl Value<'_> {
    fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
}

/// Table key. Strings are not interned: they hash by their cached hash and
/// compare by content.
#[derive(Collect, Clone, Copy, Debug)]
#[collect(no_drop)]
pub enum Key<'gc> {
    Int(i64),
    Float(u64),
    Bool(bool),
    Str(Gc<'gc, Str>),
    Table(Tab<'gc>),
    Func(Func<'gc>),
}

impl Hash for Key<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match *self {
            Key::Int(i) => h.write_i64(i),
            Key::Float(f) => h.write_u64(f ^ 1),
            Key::Bool(b) => h.write_u8(b as u8 + 2),
            Key::Str(s) => h.write_u64(s.hash),
            Key::Table(t) => h.write_usize(Gc::as_ptr(t) as *const u8 as usize),
            Key::Func(f) => h.write_usize(Gc::as_ptr(f) as *const u8 as usize),
        }
    }
}

impl PartialEq for Key<'_> {
    fn eq(&self, o: &Self) -> bool {
        match (*self, *o) {
            (Key::Int(a), Key::Int(b)) => a == b,
            (Key::Float(a), Key::Float(b)) => a == b,
            (Key::Bool(a), Key::Bool(b)) => a == b,
            (Key::Str(a), Key::Str(b)) => {
                Gc::ptr_eq(a, b) || (a.hash == b.hash && a.bytes == b.bytes)
            }
            (Key::Table(a), Key::Table(b)) => Gc::ptr_eq(a, b),
            (Key::Func(a), Key::Func(b)) => Gc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for Key<'_> {}

impl<'gc> Key<'gc> {
    fn from_value(v: Value<'gc>) -> Option<Key<'gc>> {
        Some(match v {
            Value::Nil => return None,
            Value::Bool(b) => Key::Bool(b),
            Value::Int(i) => Key::Int(i),
            Value::Float(f) => {
                if f.is_nan() {
                    return None;
                }
                if f == (f as i64) as f64 {
                    Key::Int(f as i64)
                } else {
                    Key::Float(f.to_bits())
                }
            }
            Value::Str(s) => Key::Str(s),
            Value::Table(t) => Key::Table(t),
            Value::Func(f) => Key::Func(f),
        })
    }
}

/// An ephemeron entry: the key is weak, and the value is kept alive only while
/// the key is (by resurrecting it in the finalize phase of every cycle).
#[derive(Collect, Clone, Copy, Debug)]
#[collect(no_drop)]
pub struct EphEntry<'gc> {
    pub key: GcWeak<'gc, RefLock<Table<'gc>>>,
    pub value: GcWeak<'gc, RefLock<Table<'gc>>>,
}

#[derive(Collect, Default, Debug)]
#[collect(no_drop)]
pub struct Table<'gc> {
    pub arr: Vec<Value<'gc>>,
    pub hash: HashMap<Key<'gc>, Value<'gc>, FxBuild>,
    pub meta: Option<Tab<'gc>>,
    /// Weak-key part, used only by ephemeron tables (spike: table keys and values).
    pub eph: Vec<EphEntry<'gc>>,
}

impl<'gc> Table<'gc> {
    #[inline(always)]
    fn get_int(&self, i: i64) -> Value<'gc> {
        if (i as u64).wrapping_sub(1) < self.arr.len() as u64 {
            return self.arr[(i - 1) as usize];
        }
        self.hash.get(&Key::Int(i)).copied().unwrap_or(Value::Nil)
    }
    fn get(&self, k: Key<'gc>) -> Value<'gc> {
        if let Key::Int(i) = k {
            return self.get_int(i);
        }
        self.hash.get(&k).copied().unwrap_or(Value::Nil)
    }
    fn set(&mut self, k: Key<'gc>, v: Value<'gc>) {
        if let Key::Int(i) = k {
            let n = self.arr.len() as i64;
            if i >= 1 && i <= n {
                self.arr[(i - 1) as usize] = v;
                return;
            }
            if i == n + 1 && !v.is_nil() {
                self.arr.push(v);
                if !self.hash.is_empty() {
                    let mut j = n + 2;
                    while let Some(x) = self.hash.remove(&Key::Int(j)) {
                        self.arr.push(x);
                        j += 1;
                    }
                }
                return;
            }
        }
        if v.is_nil() {
            self.hash.remove(&k);
        } else {
            self.hash.insert(k, v);
        }
    }
    fn len(&self) -> i64 {
        let mut n = self.arr.len() as i64;
        while self.hash.contains_key(&Key::Int(n + 1)) {
            n += 1;
        }
        n
    }
}

#[derive(Collect, Debug)]
#[collect(no_drop)]
pub struct FnProto<'gc> {
    #[collect(require_static)]
    pub code: Box<[Op]>,
    pub consts: Box<[Value<'gc>]>,
    pub children: Box<[Gc<'gc, FnProto<'gc>>]>,
    #[collect(require_static)]
    pub upvals: Box<[UpvalDesc]>,
    pub nregs: u8,
}

#[derive(Collect, Debug)]
#[collect(no_drop)]
pub struct Closure<'gc> {
    pub proto: Gc<'gc, FnProto<'gc>>,
    pub upvals: Box<[Upval<'gc>]>,
}

#[derive(Collect, Clone, Copy, Debug)]
#[collect(no_drop)]
pub enum UpvalState<'gc> {
    Open(usize),
    Closed(Value<'gc>),
}

#[derive(Collect, Clone, Copy, Debug)]
#[collect(no_drop)]
struct Frame<'gc> {
    func: Func<'gc>,
    pc: usize,
    base: usize,
}

#[derive(Collect, Default, Debug)]
#[collect(no_drop)]
pub struct Finalizers<'gc> {
    /// Tables whose metatable has `__gc` (weak: they must not keep objects alive).
    pub finobj: Vec<GcWeak<'gc, RefLock<Table<'gc>>>>,
    /// Resurrected tables whose finalizer is due.
    pub tobefnz: Vec<Tab<'gc>>,
    /// Registered ephemeron tables.
    pub ephemerons: Vec<GcWeak<'gc, RefLock<Table<'gc>>>>,
}

#[derive(Collect, Debug)]
#[collect(no_drop)]
pub struct State<'gc> {
    stack: Vec<Value<'gc>>,
    frames: Vec<Frame<'gc>>,
    open: Vec<(usize, Upval<'gc>)>,
    result: Option<Value<'gc>>,
    pub fin: Gc<'gc, RefLock<Finalizers<'gc>>>,
    /// Extra roots for tests.
    pub roots: Vec<Value<'gc>>,
    /// Weak probes for tests: weak handles cannot leave `mutate`, so tests
    /// park them in the root.
    pub probes: Vec<GcWeak<'gc, RefLock<Table<'gc>>>>,
}

pub type Root = Rootable![State<'_>];

enum Slice {
    Done,
    Yield,
}

type R<T> = Result<T, spike_common::VmError>;

/// Build a GC'd prototype tree from the shared bytecode.
fn load_proto<'gc>(mc: &Mutation<'gc>, p: &Proto) -> Gc<'gc, FnProto<'gc>> {
    let consts = p
        .consts
        .iter()
        .map(|c| match c {
            Const::Int(i) => Value::Int(*i),
            Const::Str(s) => Value::Str(new_str(mc, s.as_bytes())),
        })
        .collect();
    let children = p.protos.iter().map(|c| load_proto(mc, c)).collect();
    Gc::new(
        mc,
        FnProto {
            code: p.code.clone().into_boxed_slice(),
            consts,
            children,
            upvals: p.upvals.clone().into_boxed_slice(),
            nregs: p.nregs,
        },
    )
}

pub fn new_str<'gc>(mc: &Mutation<'gc>, b: &[u8]) -> Gc<'gc, Str> {
    Gc::new(
        mc,
        Str {
            hash: str_hash(b),
            bytes: b.into(),
        },
    )
}

pub fn new_arena() -> Arena<Root> {
    Arena::<Root>::new(|mc| State {
        stack: vec![Value::Nil; 256],
        frames: Vec::new(),
        open: Vec::new(),
        result: None,
        fin: Gc::new(mc, RefLock::new(Finalizers::default())),
        roots: Vec::new(),
        probes: Vec::new(),
    })
}

/// Run `p` to completion. Returns (result, GC cycles).
pub fn run(p: &Proto, superinstructions: bool) -> R<(i64, usize)> {
    run_with(p, superinstructions, false)
}

/// Like [`run`]; `stress` makes the collector run a tiny step on every allocation.
pub fn run_with(p: &Proto, superinstructions: bool, stress: bool) -> R<(i64, usize)> {
    let p = if superinstructions {
        fuse(p)
    } else {
        spike_common_clone(p)
    };
    let mut arena = new_arena();
    if stress {
        arena.metrics().set_pacing(gc_arena::metrics::Pacing {
            sleep_factor: 0.0,
            min_sleep: 1,
            mark_factor: 0.0001,
            trace_factor: 0.0001,
            keep_factor: 0.0001,
            drop_factor: 0.0001,
            free_factor: 0.0001,
        });
    }
    arena.mutate_root(|mc, st| {
        let proto = load_proto(mc, &p);
        let f = Gc::new(
            mc,
            Closure {
                proto,
                upvals: Box::new([]),
            },
        );
        st.stack[0] = Value::Func(f);
        st.push_frame(f, 1, 0)
    })?;
    let mut cycles = 0;
    loop {
        match arena.mutate_root(|mc, st| st.run_slice(mc))? {
            Slice::Done => break,
            Slice::Yield => cycles += collect_debt(&mut arena) as usize,
        }
    }
    arena.mutate(|_, st| match st.result {
        Some(Value::Int(i)) => Ok((i, cycles)),
        other => Err(format!("unexpected result {other:?}").into()),
    })
}

fn spike_common_clone(p: &Proto) -> Proto {
    Proto {
        name: p.name,
        nparams: p.nparams,
        nregs: p.nregs,
        code: p.code.clone(),
        consts: p.consts.clone(),
        protos: p.protos.clone(),
        upvals: p.upvals.clone(),
    }
}

/// Pay down allocation debt, stopping at the fully-marked point of each cycle
/// to converge ephemerons and resurrect finalizable objects.
/// Returns true when a cycle reached its atomic step.
pub fn collect_debt(arena: &mut Arena<Root>) -> bool {
    match arena.collection_phase() {
        CollectionPhase::Sweeping => {
            arena.cycle_debt();
            false
        }
        _ => {
            if arena.mark_debt().is_some() {
                atomic(arena);
                true
            } else {
                false
            }
        }
    }
}

/// Run one full collection cycle, with the atomic step.
pub fn full_gc(arena: &mut Arena<Root>) {
    if arena.collection_phase() == CollectionPhase::Sweeping {
        arena.finish_cycle();
    }
    if arena.finish_marking().is_some() {
        atomic(arena);
    }
    arena.finish_cycle();
}

/// The fully-marked step: loop finalize → re-mark until nothing is resurrected.
fn atomic(arena: &mut Arena<Root>) {
    loop {
        let marked = arena.finish_marking().expect("marked");
        let changed = marked.finalize(|fc, st| st.finalize(fc));
        if !changed {
            break;
        }
    }
    arena.finish_marking().expect("marked").start_sweeping();
}

impl<'gc> State<'gc> {
    /// Returns true if anything was resurrected (marking must continue).
    fn finalize(&self, fc: &Finalization<'gc>) -> bool {
        let mut changed = false;
        let mut fin = self.fin.borrow_mut(fc);
        // Ephemerons: a live key keeps its value.
        fin.ephemerons.retain(|w| !w.is_dead(fc));
        for w in fin.ephemerons.clone() {
            let t = w.upgrade(fc).expect("live");
            for e in t.borrow().eph.iter() {
                if !e.key.is_dead(fc) && e.value.is_dead(fc) {
                    e.value.resurrect(fc);
                    changed = true;
                }
            }
        }
        if changed {
            return true;
        }
        // __gc: resurrect unreachable finalizable objects once.
        let mut i = 0;
        while i < fin.finobj.len() {
            let w = fin.finobj[i];
            if w.is_dead(fc) {
                fin.finobj.swap_remove(i);
                let t = w.resurrect(fc).expect("not dropped yet");
                fin.tobefnz.push(t);
                changed = true;
            } else {
                i += 1;
            }
        }
        if changed {
            return true;
        }
        // Converged: clear entries whose keys died.
        for w in fin.ephemerons.clone() {
            let t = w.upgrade(fc).expect("live");
            t.borrow_mut(fc).eph.retain(|e| !e.key.is_dead(fc));
        }
        false
    }

    fn push_frame(&mut self, func: Func<'gc>, base: usize, nargs: u8) -> R<()> {
        let nregs = func.proto.nregs as usize;
        if self.frames.len() > 200_000 {
            return Err("stack overflow".into());
        }
        if self.stack.len() < base + nregs {
            self.stack.resize((base + nregs) * 2, Value::Nil);
        }
        for v in &mut self.stack[base + nargs as usize..base + nregs] {
            *v = Value::Nil;
        }
        self.frames.push(Frame { func, pc: 0, base });
        Ok(())
    }

    fn find_upval(&mut self, mc: &Mutation<'gc>, idx: usize) -> Upval<'gc> {
        let mut pos = self.open.len();
        while pos > 0 && self.open[pos - 1].0 >= idx {
            if self.open[pos - 1].0 == idx {
                return self.open[pos - 1].1;
            }
            pos -= 1;
        }
        let u = Gc::new(mc, Lock::new(UpvalState::Open(idx)));
        self.open.insert(pos, (idx, u));
        u
    }

    #[inline(always)]
    fn close_upvals(&mut self, mc: &Mutation<'gc>, level: usize) {
        while let Some(&(idx, u)) = self.open.last() {
            if idx < level {
                break;
            }
            self.open.pop();
            // `Gc::write` emits the barrier for the black-upvalue → white-value case.
            u.set(mc, UpvalState::Closed(self.stack[idx]));
        }
    }

    #[inline(always)]
    fn get_upval(&self, func: Func<'gc>, u: u8) -> Value<'gc> {
        match func.upvals[u as usize].get() {
            UpvalState::Open(i) => self.stack[i],
            UpvalState::Closed(v) => v,
        }
    }

    fn set_upval(&mut self, mc: &Mutation<'gc>, func: Func<'gc>, u: u8, v: Value<'gc>) {
        let uv = func.upvals[u as usize];
        match uv.get() {
            UpvalState::Open(i) => self.stack[i] = v,
            UpvalState::Closed(_) => uv.set(mc, UpvalState::Closed(v)),
        }
    }

    fn new_closure(
        &mut self,
        mc: &Mutation<'gc>,
        func: Func<'gc>,
        base: usize,
        p: u16,
    ) -> Value<'gc> {
        let child = func.proto.children[p as usize];
        let ups: Box<[Upval<'gc>]> = child
            .upvals
            .iter()
            .map(|d| match *d {
                UpvalDesc::Reg(r) => self.find_upval(mc, base + r as usize),
                UpvalDesc::Upval(i) => func.upvals[i as usize],
            })
            .collect();
        Value::Func(Gc::new(
            mc,
            Closure {
                proto: child,
                upvals: ups,
            },
        ))
    }

    #[inline(always)]
    fn call(&mut self, fslot: usize, nargs: u8) -> R<()> {
        let Value::Func(f) = self.stack[fslot] else {
            return Err("attempt to call a non-function value".into());
        };
        self.push_frame(f, fslot + 1, nargs)
    }

    /// Runs until the program finishes or allocation debt is due.
    fn run_slice(&mut self, mc: &Mutation<'gc>) -> R<Slice> {
        'frames: loop {
            let fr = *self.frames.last().expect("frame");
            let func = fr.func;
            let proto: &'gc FnProto<'gc> = Gc::as_ref(func.proto);
            let code: &[Op] = &proto.code;
            let k = &proto.consts;
            let base = fr.base;
            let mut pc = fr.pc;
            macro_rules! r {
                ($x:expr) => {
                    self.stack[base + $x as usize]
                };
            }
            macro_rules! gc {
                () => {
                    if mc.metrics().allocation_debt() > 0.0 {
                        self.frames.last_mut().expect("frame").pc = pc;
                        return Ok(Slice::Yield);
                    }
                };
            }
            loop {
                let op = code[pc];
                pc += 1;
                match op {
                    Op::Move { a, b } => r!(a) = r!(b),
                    Op::LoadI { a, i } => r!(a) = Value::Int(i as i64),
                    Op::LoadK { a, k: x } => r!(a) = k[x as usize],
                    Op::LoadNil { a } => r!(a) = Value::Nil,
                    Op::GetUpval { a, u } => r!(a) = self.get_upval(func, u),
                    Op::SetUpval { a, u } => self.set_upval(mc, func, u, r!(a)),
                    Op::NewTable { a } => {
                        r!(a) = Value::Table(Gc::new(mc, RefLock::new(Table::default())));
                        gc!();
                    }
                    Op::GetTable { a, b, c } => r!(a) = index(r!(b), r!(c))?,
                    Op::SetTable { a, b, c } => {
                        set_index(mc, r!(a), r!(b), r!(c))?;
                        gc!();
                    }
                    Op::GetField { a, b, k: x } => r!(a) = index(r!(b), k[x as usize])?,
                    Op::SetField { a, k: x, c } => {
                        set_index(mc, r!(a), k[x as usize], r!(c))?;
                        gc!();
                    }
                    Op::Add { a, b, c } => {
                        r!(a) = arith(r!(b), r!(c), i64::wrapping_add, |x, y| x + y)?
                    }
                    Op::Sub { a, b, c } => {
                        r!(a) = arith(r!(b), r!(c), i64::wrapping_sub, |x, y| x - y)?
                    }
                    Op::Mul { a, b, c } => {
                        r!(a) = arith(r!(b), r!(c), i64::wrapping_mul, |x, y| x * y)?
                    }
                    Op::Mod { a, b, c } => r!(a) = imod(r!(b), r!(c))?,
                    Op::AddI { a, b, i } => r!(a) = addi(r!(b), i)?,
                    Op::Concat { a, b, c } => {
                        r!(a) = concat(mc, r!(b), r!(c))?;
                        gc!();
                    }
                    Op::Len { a, b } => r!(a) = len(r!(b))?,
                    Op::Lt { a, b, k } => {
                        if lt(r!(a), r!(b))? != k {
                            pc += 1;
                        }
                    }
                    Op::Le { a, b, k } => {
                        if le(r!(a), r!(b))? != k {
                            pc += 1;
                        }
                    }
                    Op::LtI { a, i, k } => {
                        if lt(r!(a), Value::Int(i as i64))? != k {
                            pc += 1;
                        }
                    }
                    Op::Jmp { to } => pc = to as usize,
                    Op::LtJmp { a, b, k, to } => {
                        if lt(r!(a), r!(b))? == k {
                            pc = to as usize
                        } else {
                            pc += 1
                        }
                    }
                    Op::LeJmp { a, b, k, to } => {
                        if le(r!(a), r!(b))? == k {
                            pc = to as usize
                        } else {
                            pc += 1
                        }
                    }
                    Op::LtIJmp { a, i, k, to } => {
                        let c = match r!(a) {
                            Value::Int(x) => x < i as i64,
                            v => lt(v, Value::Int(i as i64))?,
                        };
                        if c == k { pc = to as usize } else { pc += 1 }
                    }
                    Op::ForPrep { a, exit } => {
                        if for_prep(&mut self.stack[base + a as usize..base + a as usize + 4])? {
                            pc = exit as usize;
                        }
                    }
                    Op::ForLoop { a, body } => {
                        let s = &mut self.stack[base + a as usize..base + a as usize + 4];
                        if let (Value::Int(count), Value::Int(i), Value::Int(step)) =
                            (s[1], s[0], s[2])
                            && count > 0
                        {
                            s[1] = Value::Int(count - 1);
                            let n = Value::Int(i.wrapping_add(step));
                            s[0] = n;
                            s[3] = n;
                            pc = body as usize;
                        }
                    }
                    Op::Closure { a, p } => {
                        r!(a) = self.new_closure(mc, func, base, p);
                        gc!();
                    }
                    Op::Close { a } => self.close_upvals(mc, base + a as usize),
                    Op::Call { a, nargs, .. } => {
                        self.frames.last_mut().expect("frame").pc = pc;
                        self.call(base + a as usize, nargs)?;
                        continue 'frames;
                    }
                    Op::CallUpvalAddI { a, u, b, i } => {
                        r!(a) = self.get_upval(func, u);
                        r!(a + 1) = addi(r!(b), i)?;
                        self.frames.last_mut().expect("frame").pc = pc + 2;
                        self.call(base + a as usize, 1)?;
                        continue 'frames;
                    }
                    Op::Return { a, n } => {
                        let v = if n == 1 { r!(a) } else { Value::Nil };
                        if self.open.last().is_some_and(|&(i, _)| i >= base) {
                            self.close_upvals(mc, base);
                        }
                        self.frames.pop();
                        self.stack[base - 1] = v;
                        if self.frames.is_empty() {
                            self.result = Some(v);
                            return Ok(Slice::Done);
                        }
                        continue 'frames;
                    }
                }
            }
        }
    }
}

// ---- helpers used by tests --------------------------------------------------

pub fn new_table<'gc>(mc: &Mutation<'gc>) -> Tab<'gc> {
    Gc::new(mc, RefLock::new(Table::default()))
}

/// `setmetatable(t, mt)`, registering `t` for finalization if `mt.__gc` is set.
pub fn set_metatable<'gc>(mc: &Mutation<'gc>, st: &State<'gc>, t: Tab<'gc>, mt: Tab<'gc>) {
    t.borrow_mut(mc).meta = Some(mt);
    let gc_key = Key::Str(new_str(mc, b"__gc"));
    if !mt.borrow().get(gc_key).is_nil() {
        st.fin.borrow_mut(mc).finobj.push(Gc::downgrade(t));
    }
}

/// Make `t` an ephemeron table (weak keys).
pub fn make_ephemeron<'gc>(mc: &Mutation<'gc>, st: &State<'gc>, t: Tab<'gc>) {
    st.fin.borrow_mut(mc).ephemerons.push(Gc::downgrade(t));
}

pub fn eph_set<'gc>(mc: &Mutation<'gc>, t: Tab<'gc>, k: Tab<'gc>, v: Tab<'gc>) {
    t.borrow_mut(mc).eph.push(EphEntry {
        key: Gc::downgrade(k),
        value: Gc::downgrade(v),
    });
}

pub fn eph_get<'gc>(mc: &Mutation<'gc>, t: Tab<'gc>, k: Tab<'gc>) -> Option<Tab<'gc>> {
    let tb = t.borrow();
    let e = tb
        .eph
        .iter()
        .find(|e| GcWeak::ptr_eq(e.key, Gc::downgrade(k)))?;
    e.value.upgrade(mc)
}

pub fn tab_set<'gc>(mc: &Mutation<'gc>, t: Tab<'gc>, k: Value<'gc>, v: Value<'gc>) {
    t.borrow_mut(mc).set(Key::from_value(k).expect("key"), v);
}

pub fn tab_get<'gc>(t: Tab<'gc>, k: Value<'gc>) -> Value<'gc> {
    t.borrow().get(Key::from_value(k).expect("key"))
}

impl<'gc> State<'gc> {
    pub fn push_root(&mut self, v: Value<'gc>) {
        self.roots.push(v);
    }
}

// ---- semantics helpers -----------------------------------------------------

#[inline(always)]
fn index<'gc>(t: Value<'gc>, k: Value<'gc>) -> R<Value<'gc>> {
    let Value::Table(t) = t else {
        return Err("attempt to index a non-table value".into());
    };
    let t = t.borrow();
    if let Value::Int(i) = k {
        return Ok(t.get_int(i));
    }
    Ok(Key::from_value(k).map_or(Value::Nil, |k| t.get(k)))
}

#[inline(always)]
fn set_index<'gc>(mc: &Mutation<'gc>, t: Value<'gc>, k: Value<'gc>, v: Value<'gc>) -> R<()> {
    let Value::Table(t) = t else {
        return Err("attempt to index a non-table value".into());
    };
    let k = Key::from_value(k).ok_or("invalid table key")?;
    t.borrow_mut(mc).set(k, v);
    Ok(())
}

fn concat<'gc>(mc: &Mutation<'gc>, a: Value<'gc>, b: Value<'gc>) -> R<Value<'gc>> {
    let mut out = Vec::new();
    for v in [a, b] {
        match v {
            Value::Str(s) => out.extend_from_slice(&s.bytes),
            Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
            Value::Float(f) => out.extend_from_slice(format!("{f}").as_bytes()),
            _ => return Err("attempt to concatenate".into()),
        }
    }
    Ok(Value::Str(new_str(mc, &out)))
}

fn len(v: Value<'_>) -> R<Value<'_>> {
    match v {
        Value::Str(s) => Ok(Value::Int(s.bytes.len() as i64)),
        Value::Table(t) => Ok(Value::Int(t.borrow().len())),
        _ => Err("attempt to get length".into()),
    }
}

#[inline(always)]
fn arith<'gc>(
    a: Value<'gc>,
    b: Value<'gc>,
    fi: fn(i64, i64) -> i64,
    ff: fn(f64, f64) -> f64,
) -> R<Value<'gc>> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(fi(x, y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(ff(x, y))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(ff(x as f64, y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(ff(x, y as f64))),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

#[inline(always)]
fn addi(a: Value<'_>, i: i16) -> R<Value<'_>> {
    match a {
        Value::Int(x) => Ok(Value::Int(x.wrapping_add(i as i64))),
        Value::Float(x) => Ok(Value::Float(x + i as f64)),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

fn imod<'gc>(a: Value<'gc>, b: Value<'gc>) -> R<Value<'gc>> {
    match (a, b) {
        (Value::Int(_), Value::Int(0)) => Err("attempt to perform 'n%%0'".into()),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(lua_imod(x, y))),
        _ => Err("mod: only integers in the spike".into()),
    }
}

#[inline(always)]
fn lt(a: Value<'_>, b: Value<'_>) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x < y),
        (Value::Float(x), Value::Float(y)) => Ok(x < y),
        (Value::Int(x), Value::Float(y)) => Ok((x as f64) < y),
        (Value::Float(x), Value::Int(y)) => Ok(x < y as f64),
        _ => Err("attempt to compare".into()),
    }
}

#[inline(always)]
fn le(a: Value<'_>, b: Value<'_>) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x <= y),
        (Value::Float(x), Value::Float(y)) => Ok(x <= y),
        (Value::Int(x), Value::Float(y)) => Ok((x as f64) <= y),
        (Value::Float(x), Value::Int(y)) => Ok(x <= y as f64),
        _ => Err("attempt to compare".into()),
    }
}

fn for_prep(s: &mut [Value<'_>]) -> R<bool> {
    match (s[0], s[1], s[2]) {
        (Value::Int(init), Value::Int(limit), Value::Int(step)) => {
            if step == 0 {
                return Err("'for' step is zero".into());
            }
            if (step > 0 && init > limit) || (step < 0 && init < limit) {
                return Ok(true);
            }
            let count = if step > 0 {
                (limit as u64).wrapping_sub(init as u64) / step as u64
            } else {
                (init as u64).wrapping_sub(limit as u64) / (-(step + 1) as u64 + 1)
            };
            s[1] = Value::Int(count as i64);
            s[3] = Value::Int(init);
            Ok(false)
        }
        _ => Err("'for' loop: only integer loops in the spike".into()),
    }
}
