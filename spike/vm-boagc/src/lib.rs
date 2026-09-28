//! Micro-VM on boa_gc 0.22 (ADR-2 option C, added after the first round).
//!
//! Same bytecode and semantics as `spike-vm-arena`. boa_gc finds roots by
//! reference counting: every `Gc` held outside the GC heap (the VM stack,
//! Rust locals) is a root, so the interpreter needs no rooting discipline and
//! no safe points. The price is that `Gc` is `Clone`, not `Copy`: every value
//! copy bumps a count, and every overwrite drops one.
//!
//! Collection is stop-the-world and runs inside `Gc::new` once the allocated
//! bytes pass a threshold (1 MiB, then 100/70 of the live size). The heap is a
//! thread-local shared by every VM on the thread. `stress` forces a full
//! collection at every allocating instruction instead.

use boa_gc::{Ephemeron, Finalize, Gc, GcRefCell, Trace, WeakGc, force_collect};
use spike_common::{Const, FxBuild, Op, Proto, UpvalDesc, fuse, lua_imod, str_hash};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

#[derive(Trace, Finalize, Debug)]
pub struct Str {
    pub hash: u64,
    pub bytes: Box<[u8]>,
}

pub type Tab = Gc<GcRefCell<Table>>;
pub type Func = Gc<Closure>;
pub type Upval = Gc<GcRefCell<UpvalState>>;

#[derive(Trace, Finalize, Clone, Debug)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Gc<Str>),
    Table(Tab),
    Func(Func),
}

impl Value {
    fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }
}

fn addr<T: Trace>(g: &Gc<T>) -> usize {
    std::ptr::from_ref::<T>(g) as usize
}

/// Table key. Strings are not interned: they hash by their cached hash and
/// compare by content.
#[derive(Trace, Finalize, Clone, Debug)]
pub enum Key {
    Int(i64),
    Float(u64),
    Bool(bool),
    Str(Gc<Str>),
    Table(Tab),
    Func(Func),
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            Key::Int(i) => h.write_i64(*i),
            Key::Float(f) => h.write_u64(f ^ 1),
            Key::Bool(b) => h.write_u8(*b as u8 + 2),
            Key::Str(s) => h.write_u64(s.hash),
            Key::Table(t) => h.write_usize(addr(t)),
            Key::Func(f) => h.write_usize(addr(f)),
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, o: &Self) -> bool {
        match (self, o) {
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
impl Eq for Key {}

impl Key {
    fn from_value(v: &Value) -> Option<Key> {
        Some(match v {
            Value::Nil => return None,
            Value::Bool(b) => Key::Bool(*b),
            Value::Int(i) => Key::Int(*i),
            Value::Float(f) => {
                let f = *f;
                if f.is_nan() {
                    return None;
                }
                if f == (f as i64) as f64 {
                    Key::Int(f as i64)
                } else {
                    Key::Float(f.to_bits())
                }
            }
            Value::Str(s) => Key::Str(s.clone()),
            Value::Table(t) => Key::Table(t.clone()),
            Value::Func(f) => Key::Func(f.clone()),
        })
    }
}

thread_local! {
    /// Resurrected tables whose `__gc` is due (Lua's `tobefnz`). The queue
    /// holds strong handles, so its entries are roots.
    static TOBEFNZ: RefCell<Vec<Tab>> = const { RefCell::new(Vec::new()) };
}

/// Take the tables whose finalizer is due.
pub fn take_tobefnz() -> Vec<Tab> {
    TOBEFNZ.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

#[derive(Trace, Default, Debug)]
pub struct Table {
    pub arr: Vec<Value>,
    pub hash: HashMap<Key, Value, FxBuild>,
    pub meta: Option<Tab>,
    /// Weak-key part, used only by ephemeron tables (spike: table keys and values).
    pub eph: Vec<Ephemeron<GcRefCell<Table>, Tab>>,
    /// Set by `setmetatable` when the metatable has `__gc`; cleared once queued.
    pub has_gc: Cell<bool>,
    /// A weak handle to this table, so its finalizer can resurrect it.
    pub self_weak: Option<WeakGc<GcRefCell<Table>>>,
}

impl Finalize for Table {
    /// Runs inside the collector for every unreachable table. It must not
    /// allocate GC objects (the heap is borrowed), so it only queues.
    fn finalize(&self) {
        if self.has_gc.get()
            && let Some(t) = self.self_weak.as_ref().and_then(WeakGc::upgrade)
        {
            self.has_gc.set(false);
            TOBEFNZ.with(|q| q.borrow_mut().push(t));
        }
    }
}

impl Table {
    #[inline(always)]
    fn get_int(&self, i: i64) -> Value {
        if (i as u64).wrapping_sub(1) < self.arr.len() as u64 {
            return self.arr[(i - 1) as usize].clone();
        }
        self.hash.get(&Key::Int(i)).cloned().unwrap_or(Value::Nil)
    }
    fn get(&self, k: &Key) -> Value {
        if let Key::Int(i) = k {
            return self.get_int(*i);
        }
        self.hash.get(k).cloned().unwrap_or(Value::Nil)
    }
    fn set(&mut self, k: Key, v: Value) {
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

#[derive(Trace, Finalize, Debug)]
pub struct FnProto {
    #[unsafe_ignore_trace]
    pub code: Box<[Op]>,
    pub consts: Box<[Value]>,
    pub children: Box<[Gc<FnProto>]>,
    #[unsafe_ignore_trace]
    pub upvals: Box<[UpvalDesc]>,
    pub nregs: u8,
}

#[derive(Trace, Finalize, Debug)]
pub struct Closure {
    pub proto: Gc<FnProto>,
    pub upvals: Box<[Upval]>,
}

#[derive(Trace, Finalize, Clone, Debug)]
pub enum UpvalState {
    Open(usize),
    Closed(Value),
}

#[derive(Clone, Debug)]
struct Frame {
    func: Func,
    pc: usize,
    base: usize,
}

type R<T> = Result<T, spike_common::VmError>;

/// Plain Rust struct: everything it holds is a root.
pub struct Vm {
    stack: Vec<Value>,
    frames: Vec<Frame>,
    open: Vec<(usize, Upval)>,
    stress: bool,
    /// Extra roots for tests.
    pub roots: Vec<Value>,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

fn load_proto(p: &Proto) -> Gc<FnProto> {
    let consts = p
        .consts
        .iter()
        .map(|c| match c {
            Const::Int(i) => Value::Int(*i),
            Const::Str(s) => Value::Str(new_str(s.as_bytes())),
        })
        .collect();
    let children = p.protos.iter().map(|c| load_proto(c)).collect();
    Gc::new(FnProto {
        code: p.code.clone().into_boxed_slice(),
        consts,
        children,
        upvals: p.upvals.clone().into_boxed_slice(),
        nregs: p.nregs,
    })
}

pub fn new_str(b: &[u8]) -> Gc<Str> {
    Gc::new(Str {
        hash: str_hash(b),
        bytes: b.into(),
    })
}

/// Run `p` to completion. Returns (result, GC cycles); boa_gc does not expose
/// its cycle count, so the count is always 0.
pub fn run(p: &Proto, superinstructions: bool) -> R<(i64, usize)> {
    run_with(p, superinstructions, false)
}

/// Like [`run`]; `stress` runs a full collection at every allocating instruction.
pub fn run_with(p: &Proto, superinstructions: bool, stress: bool) -> R<(i64, usize)> {
    let p = if superinstructions {
        fuse(p)
    } else {
        Proto {
            name: p.name,
            nparams: p.nparams,
            nregs: p.nregs,
            code: p.code.clone(),
            consts: p.consts.clone(),
            protos: p.protos.clone(),
            upvals: p.upvals.clone(),
        }
    };
    let mut vm = Vm::new();
    vm.stress = stress;
    match vm.run(&p)? {
        Value::Int(i) => Ok((i, 0)),
        other => Err(format!("unexpected result {other:?}").into()),
    }
}

/// Run one full collection (all VMs on this thread share the heap).
pub fn full_gc() {
    force_collect();
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            stack: vec![Value::Nil; 256],
            frames: Vec::new(),
            open: Vec::new(),
            stress: false,
            roots: Vec::new(),
        }
    }

    pub fn run(&mut self, p: &Proto) -> R<Value> {
        let proto = load_proto(p);
        let f = Gc::new(Closure {
            proto,
            upvals: Box::new([]),
        });
        self.stack[0] = Value::Func(f.clone());
        self.push_frame(f, 1, 0)?;
        self.execute()
    }

    fn push_frame(&mut self, func: Func, base: usize, nargs: u8) -> R<()> {
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

    fn find_upval(&mut self, idx: usize) -> Upval {
        let mut pos = self.open.len();
        while pos > 0 && self.open[pos - 1].0 >= idx {
            if self.open[pos - 1].0 == idx {
                return self.open[pos - 1].1.clone();
            }
            pos -= 1;
        }
        let u = Gc::new(GcRefCell::new(UpvalState::Open(idx)));
        self.open.insert(pos, (idx, u.clone()));
        u
    }

    #[inline(always)]
    fn close_upvals(&mut self, level: usize) {
        while let Some((idx, _)) = self.open.last() {
            if *idx < level {
                break;
            }
            let (idx, u) = self.open.pop().expect("open");
            *u.borrow_mut() = UpvalState::Closed(self.stack[idx].clone());
        }
    }

    #[inline(always)]
    fn get_upval(&self, func: &Closure, u: u8) -> Value {
        match &*func.upvals[u as usize].borrow() {
            UpvalState::Open(i) => self.stack[*i].clone(),
            UpvalState::Closed(v) => v.clone(),
        }
    }

    fn set_upval(&mut self, func: &Closure, u: u8, v: Value) {
        let mut uv = func.upvals[u as usize].borrow_mut();
        match &mut *uv {
            UpvalState::Open(i) => self.stack[*i] = v,
            UpvalState::Closed(c) => *c = v,
        }
    }

    fn new_closure(&mut self, func: &Closure, base: usize, p: u16) -> Value {
        let child = func.proto.children[p as usize].clone();
        let ups: Box<[Upval]> = child
            .upvals
            .iter()
            .map(|d| match *d {
                UpvalDesc::Reg(r) => self.find_upval(base + r as usize),
                UpvalDesc::Upval(i) => func.upvals[i as usize].clone(),
            })
            .collect();
        Value::Func(Gc::new(Closure {
            proto: child,
            upvals: ups,
        }))
    }

    #[inline(always)]
    fn call(&mut self, fslot: usize, nargs: u8) -> R<()> {
        let Value::Func(f) = &self.stack[fslot] else {
            return Err("attempt to call a non-function value".into());
        };
        let f = f.clone();
        self.push_frame(f, fslot + 1, nargs)
    }

    fn execute(&mut self) -> R<Value> {
        'frames: loop {
            let fr = self.frames.last().expect("frame");
            let func = fr.func.clone();
            let base = fr.base;
            let mut pc = fr.pc;
            let proto: &FnProto = &func.proto;
            let code: &[Op] = &proto.code;
            let k = &proto.consts;
            macro_rules! r {
                ($x:expr) => {
                    self.stack[base + $x as usize]
                };
            }
            macro_rules! gc {
                () => {
                    if self.stress {
                        force_collect();
                    }
                };
            }
            loop {
                let op = code[pc];
                pc += 1;
                match op {
                    Op::Move { a, b } => r!(a) = r!(b).clone(),
                    Op::LoadI { a, i } => r!(a) = Value::Int(i as i64),
                    Op::LoadK { a, k: x } => r!(a) = k[x as usize].clone(),
                    Op::LoadNil { a } => r!(a) = Value::Nil,
                    Op::GetUpval { a, u } => r!(a) = self.get_upval(&func, u),
                    Op::SetUpval { a, u } => {
                        let v = r!(a).clone();
                        self.set_upval(&func, u, v)
                    }
                    Op::NewTable { a } => {
                        r!(a) = Value::Table(Gc::new(GcRefCell::new(Table::default())));
                        gc!();
                    }
                    Op::GetTable { a, b, c } => r!(a) = index(&r!(b), &r!(c))?,
                    Op::SetTable { a, b, c } => {
                        set_index(&r!(a), &r!(b), r!(c).clone())?;
                        gc!();
                    }
                    Op::GetField { a, b, k: x } => r!(a) = index(&r!(b), &k[x as usize])?,
                    Op::SetField { a, k: x, c } => {
                        set_index(&r!(a), &k[x as usize], r!(c).clone())?;
                        gc!();
                    }
                    Op::Add { a, b, c } => {
                        r!(a) = arith(&r!(b), &r!(c), i64::wrapping_add, |x, y| x + y)?
                    }
                    Op::Sub { a, b, c } => {
                        r!(a) = arith(&r!(b), &r!(c), i64::wrapping_sub, |x, y| x - y)?
                    }
                    Op::Mul { a, b, c } => {
                        r!(a) = arith(&r!(b), &r!(c), i64::wrapping_mul, |x, y| x * y)?
                    }
                    Op::Mod { a, b, c } => r!(a) = imod(&r!(b), &r!(c))?,
                    Op::AddI { a, b, i } => r!(a) = addi(&r!(b), i)?,
                    Op::Concat { a, b, c } => {
                        r!(a) = concat(&r!(b), &r!(c))?;
                        gc!();
                    }
                    Op::Len { a, b } => r!(a) = len(&r!(b))?,
                    Op::Lt { a, b, k } => {
                        if lt(&r!(a), &r!(b))? != k {
                            pc += 1;
                        }
                    }
                    Op::Le { a, b, k } => {
                        if le(&r!(a), &r!(b))? != k {
                            pc += 1;
                        }
                    }
                    Op::LtI { a, i, k } => {
                        if lt(&r!(a), &Value::Int(i as i64))? != k {
                            pc += 1;
                        }
                    }
                    Op::Jmp { to } => pc = to as usize,
                    Op::LtJmp { a, b, k, to } => {
                        if lt(&r!(a), &r!(b))? == k {
                            pc = to as usize
                        } else {
                            pc += 1
                        }
                    }
                    Op::LeJmp { a, b, k, to } => {
                        if le(&r!(a), &r!(b))? == k {
                            pc = to as usize
                        } else {
                            pc += 1
                        }
                    }
                    Op::LtIJmp { a, i, k, to } => {
                        let c = match &r!(a) {
                            Value::Int(x) => *x < i as i64,
                            v => lt(v, &Value::Int(i as i64))?,
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
                            (&s[1], &s[0], &s[2])
                            && *count > 0
                        {
                            let (count, n) = (*count - 1, i.wrapping_add(*step));
                            s[1] = Value::Int(count);
                            s[0] = Value::Int(n);
                            s[3] = Value::Int(n);
                            pc = body as usize;
                        }
                    }
                    Op::Closure { a, p } => {
                        r!(a) = self.new_closure(&func, base, p);
                        gc!();
                    }
                    Op::Close { a } => self.close_upvals(base + a as usize),
                    Op::Call { a, nargs, .. } => {
                        self.frames.last_mut().expect("frame").pc = pc;
                        self.call(base + a as usize, nargs)?;
                        continue 'frames;
                    }
                    Op::CallUpvalAddI { a, u, b, i } => {
                        r!(a) = self.get_upval(&func, u);
                        r!(a + 1) = addi(&r!(b), i)?;
                        self.frames.last_mut().expect("frame").pc = pc + 2;
                        self.call(base + a as usize, 1)?;
                        continue 'frames;
                    }
                    Op::Return { a, n } => {
                        let v = if n == 1 { r!(a).clone() } else { Value::Nil };
                        if self.open.last().is_some_and(|&(i, _)| i >= base) {
                            self.close_upvals(base);
                        }
                        self.frames.pop();
                        if self.frames.is_empty() {
                            return Ok(v);
                        }
                        self.stack[base - 1] = v;
                        continue 'frames;
                    }
                }
            }
        }
    }
}

// ---- helpers used by tests --------------------------------------------------

pub fn new_table() -> Tab {
    Gc::new(GcRefCell::new(Table::default()))
}

/// `setmetatable(t, mt)`, arming `t`'s finalizer if `mt.__gc` is set.
pub fn set_metatable(t: &Tab, mt: &Tab) {
    let has_gc = !mt.borrow().get(&Key::Str(new_str(b"__gc"))).is_nil();
    let weak = has_gc.then(|| WeakGc::new(t));
    let mut tb = t.borrow_mut();
    tb.meta = Some(mt.clone());
    if has_gc {
        tb.has_gc.set(true);
        tb.self_weak = weak;
    }
}

pub fn eph_set(t: &Tab, k: &Tab, v: Tab) {
    let e = Ephemeron::new(k, v);
    t.borrow_mut().eph.push(e);
}

pub fn eph_get(t: &Tab, k: &Tab) -> Option<Tab> {
    let tb = t.borrow();
    tb.eph
        .iter()
        .find(|e| e.key().is_some_and(|ek| Gc::ptr_eq(&ek, k)))?
        .value()
        .map(|v| v.clone())
}

/// Live entries (boa_gc clears dead ones in place; the spike does not compact).
pub fn eph_live(t: &Tab) -> usize {
    t.borrow().eph.iter().filter(|e| e.has_value()).count()
}

pub fn tab_set(t: &Tab, k: Value, v: Value) {
    t.borrow_mut().set(Key::from_value(&k).expect("key"), v);
}

pub fn tab_get(t: &Tab, k: &Value) -> Value {
    t.borrow().get(&Key::from_value(k).expect("key"))
}

// ---- semantics helpers -----------------------------------------------------

#[inline(always)]
fn index(t: &Value, k: &Value) -> R<Value> {
    let Value::Table(t) = t else {
        return Err("attempt to index a non-table value".into());
    };
    let t = t.borrow();
    if let Value::Int(i) = k {
        return Ok(t.get_int(*i));
    }
    Ok(Key::from_value(k).map_or(Value::Nil, |k| t.get(&k)))
}

#[inline(always)]
fn set_index(t: &Value, k: &Value, v: Value) -> R<()> {
    let Value::Table(t) = t else {
        return Err("attempt to index a non-table value".into());
    };
    let k = Key::from_value(k).ok_or("invalid table key")?;
    t.borrow_mut().set(k, v);
    Ok(())
}

fn concat(a: &Value, b: &Value) -> R<Value> {
    let mut out = Vec::new();
    for v in [a, b] {
        match v {
            Value::Str(s) => out.extend_from_slice(&s.bytes),
            Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
            Value::Float(f) => out.extend_from_slice(format!("{f}").as_bytes()),
            _ => return Err("attempt to concatenate".into()),
        }
    }
    Ok(Value::Str(new_str(&out)))
}

fn len(v: &Value) -> R<Value> {
    match v {
        Value::Str(s) => Ok(Value::Int(s.bytes.len() as i64)),
        Value::Table(t) => Ok(Value::Int(t.borrow().len())),
        _ => Err("attempt to get length".into()),
    }
}

#[inline(always)]
fn arith(a: &Value, b: &Value, fi: fn(i64, i64) -> i64, ff: fn(f64, f64) -> f64) -> R<Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(fi(*x, *y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(ff(*x, *y))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(ff(*x as f64, *y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(ff(*x, *y as f64))),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

#[inline(always)]
fn addi(a: &Value, i: i16) -> R<Value> {
    match a {
        Value::Int(x) => Ok(Value::Int(x.wrapping_add(i as i64))),
        Value::Float(x) => Ok(Value::Float(x + i as f64)),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

fn imod(a: &Value, b: &Value) -> R<Value> {
    match (a, b) {
        (Value::Int(_), Value::Int(0)) => Err("attempt to perform 'n%%0'".into()),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(lua_imod(*x, *y))),
        _ => Err("mod: only integers in the spike".into()),
    }
}

#[inline(always)]
fn lt(a: &Value, b: &Value) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x < y),
        (Value::Float(x), Value::Float(y)) => Ok(x < y),
        (Value::Int(x), Value::Float(y)) => Ok((*x as f64) < *y),
        (Value::Float(x), Value::Int(y)) => Ok(*x < *y as f64),
        _ => Err("attempt to compare".into()),
    }
}

#[inline(always)]
fn le(a: &Value, b: &Value) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x <= y),
        (Value::Float(x), Value::Float(y)) => Ok(x <= y),
        (Value::Int(x), Value::Float(y)) => Ok((*x as f64) <= *y),
        (Value::Float(x), Value::Int(y)) => Ok(*x <= *y as f64),
        _ => Err("attempt to compare".into()),
    }
}

fn for_prep(s: &mut [Value]) -> R<bool> {
    match (&s[0], &s[1], &s[2]) {
        (Value::Int(init), Value::Int(limit), Value::Int(step)) => {
            let (init, limit, step) = (*init, *limit, *step);
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
