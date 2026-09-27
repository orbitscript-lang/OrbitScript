//! Micro-VM on safe typed arenas with `u32` handles (ADR-2 option B).
//!
//! Stackless: Lua frames live in a `Vec`, registers in one `Vec<Value>`.
//! Three dispatch modes share the same semantics helpers:
//! plain `match`, `match` over superinstruction-fused bytecode, and
//! closure-threaded code (one boxed closure per instruction).

pub mod heap;
pub mod value;

use heap::{Heap, Roots, Upval};
use spike_common::{Const, Op, Proto, UpvalDesc, fuse, lua_imod};
use std::rc::Rc;
pub use value::{FnRef, Key, StrRef, TabRef, UvRef, Value};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dispatch {
    Match,
    Super,
    Closure,
}

type Thunk = Box<dyn Fn(&mut Vm, &mut usize, usize, FnRef) -> R<Ctl>>;

enum Ctl {
    Next,
    Call { a: u8, skip: usize },
    Ret { a: u8, n: u8 },
}

/// A prototype prepared for one dispatch mode.
pub struct CProto {
    pub p: Proto,
    consts: Vec<Value>,
    threaded: Vec<Thunk>,
    children: Vec<u32>,
}

impl std::fmt::Debug for CProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CProto").field("p", &self.p.name).finish()
    }
}

#[derive(Debug)]
struct Frame {
    proto: u32,
    func: FnRef,
    pc: u32,
    base: u32,
}

#[derive(Debug)]
pub struct Vm {
    pub heap: Heap<u32>,
    /// Immutable code store, indexed by proto id. Frames and closures hold ids,
    /// so calls touch no reference counts.
    protos: Rc<Vec<CProto>>,
    pub stack: Vec<Value>,
    frames: Vec<Frame>,
    open: Vec<(u32, UvRef)>,
    /// Extra GC roots (used by tests).
    pub roots: Vec<Value>,
    dispatch: Dispatch,
}

type R<T> = Result<T, spike_common::VmError>;

impl Vm {
    pub fn new(dispatch: Dispatch) -> Self {
        Vm {
            heap: Heap::new(),
            stack: vec![Value::Nil; 256],
            frames: Vec::new(),
            open: Vec::new(),
            roots: Vec::new(),
            protos: Rc::new(Vec::new()),
            dispatch,
        }
    }

    /// Load a program; returns the id of its main prototype.
    pub fn prepare(&mut self, p: &Proto) -> u32 {
        let p = if self.dispatch == Dispatch::Super {
            fuse(p)
        } else {
            clone_proto(p)
        };
        let mut store = std::mem::take(Rc::get_mut(&mut self.protos).expect("not running"));
        let id = self.prepare_rec(p, &mut store);
        self.protos = Rc::new(store);
        id
    }

    fn prepare_rec(&mut self, p: Proto, store: &mut Vec<CProto>) -> u32 {
        let consts = p
            .consts
            .iter()
            .map(|c| match c {
                Const::Int(i) => Value::Int(*i),
                Const::Str(s) => Value::Str(self.heap.new_str(s.as_bytes())),
            })
            .collect::<Vec<_>>();
        // Constants are kept alive as extra roots (the spike has no GC'd protos).
        self.roots.extend(consts.iter().copied());
        let children = p
            .protos
            .iter()
            .map(|c| self.prepare_rec(clone_proto(c), store))
            .collect();
        let threaded = if self.dispatch == Dispatch::Closure {
            p.code.iter().map(|&op| thread_op(op, &consts)).collect()
        } else {
            Vec::new()
        };
        store.push(CProto {
            p,
            consts,
            threaded,
            children,
        });
        (store.len() - 1) as u32
    }

    pub fn run(&mut self, main: u32) -> R<Value> {
        let f = self.heap.new_closure(main, Box::new([]));
        self.stack[0] = Value::Func(f);
        let nregs = self.protos[main as usize].p.nregs;
        self.push_frame(f, main, nregs, 1, 0)?;
        match self.dispatch {
            Dispatch::Closure => self.exec_threaded(),
            _ => self.exec_match(),
        }
    }

    #[inline(always)]
    fn push_frame(&mut self, func: FnRef, proto: u32, nregs: u8, base: usize, nargs: u8) -> R<()> {
        let nregs = nregs as usize;
        if self.frames.len() > 200_000 {
            return Err("stack overflow".into());
        }
        if self.stack.len() < base + nregs {
            self.stack.resize((base + nregs) * 2, Value::Nil);
        }
        for v in &mut self.stack[base + nargs as usize..base + nregs] {
            *v = Value::Nil;
        }
        self.frames.push(Frame {
            proto,
            func,
            pc: 0,
            base: base as u32,
        });
        Ok(())
    }

    fn gc_step(&mut self, top: usize) {
        let roots = Roots {
            stack: &self.stack[..top],
            open: &self.open,
            extra: &self.roots,
        };
        self.heap.step(&roots);
    }

    pub fn full_gc(&mut self) {
        let top = self.frames.last().map_or(0, |f| {
            f.base as usize + self.protos[f.proto as usize].p.nregs as usize
        });
        let roots = Roots {
            stack: &self.stack[..top],
            open: &self.open,
            extra: &self.roots,
        };
        self.heap.full_gc(&roots);
    }

    // ---- shared semantics ----------------------------------------------

    #[inline(always)]
    fn get_upval(&self, func: FnRef, u: u8) -> Value {
        let uv = self.heap.fns.get(func.0).upvals[u as usize];
        match *self.heap.uvs.get(uv.0) {
            Upval::Open(i) => self.stack[i as usize],
            Upval::Closed(v) => v,
        }
    }

    fn set_upval(&mut self, func: FnRef, u: u8, v: Value) {
        let uv = self.heap.fns.get(func.0).upvals[u as usize];
        match *self.heap.uvs.get(uv.0) {
            Upval::Open(i) => self.stack[i as usize] = v,
            Upval::Closed(_) => {
                *self.heap.uvs.get_mut(uv.0) = Upval::Closed(v);
                self.heap.barrier_fwd_uv(uv, v);
            }
        }
    }

    fn find_upval(&mut self, idx: u32) -> UvRef {
        let mut pos = self.open.len();
        while pos > 0 && self.open[pos - 1].0 >= idx {
            if self.open[pos - 1].0 == idx {
                return self.open[pos - 1].1;
            }
            pos -= 1;
        }
        let u = self.heap.new_upval(Upval::Open(idx));
        self.open.insert(pos, (idx, u));
        u
    }

    #[inline(always)]
    fn close_upvals(&mut self, level: usize) {
        while let Some(&(idx, u)) = self.open.last() {
            if (idx as usize) < level {
                break;
            }
            self.open.pop();
            let v = self.stack[idx as usize];
            *self.heap.uvs.get_mut(u.0) = Upval::Closed(v);
            self.heap.barrier_fwd_uv(u, v);
        }
    }

    fn new_closure(&mut self, parent: &CProto, func: FnRef, base: usize, p: u16) -> Value {
        let child_id = parent.children[p as usize];
        let child = &self.protos.clone()[child_id as usize];
        let ups: Box<[UvRef]> = child
            .p
            .upvals
            .iter()
            .map(|d| match *d {
                UpvalDesc::Reg(r) => self.find_upval((base + r as usize) as u32),
                UpvalDesc::Upval(i) => self.heap.fns.get(func.0).upvals[i as usize],
            })
            .collect();
        Value::Func(self.heap.new_closure(child_id, ups))
    }

    #[inline(always)]
    fn index(&mut self, t: Value, k: Value) -> R<Value> {
        let Value::Table(t) = t else {
            return Err("attempt to index a non-table value".into());
        };
        if let Value::Int(i) = k {
            return Ok(self.heap.get_int(t, i));
        }
        match self.heap.key(k) {
            Some(k) => Ok(self.heap.get(t, k)),
            None => Ok(Value::Nil),
        }
    }

    #[inline(always)]
    fn set_index(&mut self, t: Value, k: Value, v: Value) -> R<()> {
        let Value::Table(t) = t else {
            return Err("attempt to index a non-table value".into());
        };
        match self.heap.key(k) {
            Some(k) => {
                self.heap.set(t, k, v);
                Ok(())
            }
            None => Err("invalid table key".into()),
        }
    }

    fn concat(&mut self, a: Value, b: Value) -> R<Value> {
        let mut out = Vec::new();
        for v in [a, b] {
            match v {
                Value::Str(s) => out.extend_from_slice(self.heap.str_bytes(s)),
                Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
                Value::Float(f) => out.extend_from_slice(format!("{f}").as_bytes()),
                _ => return Err("attempt to concatenate".into()),
            }
        }
        Ok(Value::Str(self.heap.new_str(&out)))
    }

    fn len(&self, v: Value) -> R<Value> {
        match v {
            Value::Str(s) => Ok(Value::Int(self.heap.str_bytes(s).len() as i64)),
            Value::Table(t) => Ok(Value::Int(self.heap.len(t))),
            _ => Err("attempt to get length".into()),
        }
    }

    // ---- match dispatch --------------------------------------------------

    fn exec_match(&mut self) -> R<Value> {
        let protos = self.protos.clone();
        'frames: loop {
            let fr = self.frames.last().expect("frame");
            let proto = &protos[fr.proto as usize];
            let code: &[Op] = &proto.p.code;
            let k = &proto.consts;
            let func = fr.func;
            let base = fr.base as usize;
            let top = base + proto.p.nregs as usize;
            let mut pc = fr.pc as usize;
            macro_rules! r {
                ($x:expr) => {
                    self.stack[base + $x as usize]
                };
            }
            macro_rules! gc {
                () => {
                    if self.heap.debt > 0 {
                        self.gc_step(top);
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
                    Op::SetUpval { a, u } => self.set_upval(func, u, r!(a)),
                    Op::NewTable { a } => {
                        r!(a) = Value::Table(self.heap.new_table());
                        gc!();
                    }
                    Op::GetTable { a, b, c } => r!(a) = self.index(r!(b), r!(c))?,
                    Op::SetTable { a, b, c } => {
                        self.set_index(r!(a), r!(b), r!(c))?;
                        gc!();
                    }
                    Op::GetField { a, b, k: x } => r!(a) = self.index(r!(b), k[x as usize])?,
                    Op::SetField { a, k: x, c } => {
                        self.set_index(r!(a), k[x as usize], r!(c))?;
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
                        r!(a) = self.concat(r!(b), r!(c))?;
                        gc!();
                    }
                    Op::Len { a, b } => r!(a) = self.len(r!(b))?,
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
                        r!(a) = self.new_closure(proto, func, base, p);
                        gc!();
                    }
                    Op::Close { a } => self.close_upvals(base + a as usize),
                    Op::Call { a, nargs, .. } => {
                        self.frames.last_mut().expect("frame").pc = pc as u32;
                        self.call(base + a as usize, nargs)?;
                        continue 'frames;
                    }
                    Op::CallUpvalAddI { a, u, b, i } => {
                        r!(a) = self.get_upval(func, u);
                        r!(a + 1) = addi(r!(b), i)?;
                        self.frames.last_mut().expect("frame").pc = (pc + 2) as u32;
                        self.call(base + a as usize, 1)?;
                        continue 'frames;
                    }
                    Op::Return { a, n } => {
                        let v = if n == 1 { r!(a) } else { Value::Nil };
                        if self.do_return(base, v) {
                            return Ok(v);
                        }
                        continue 'frames;
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn call(&mut self, fslot: usize, nargs: u8) -> R<()> {
        let Value::Func(f) = self.stack[fslot] else {
            return Err("attempt to call a non-function value".into());
        };
        let proto = self.heap.fns.get(f.0).proto;
        let nregs = self.protos[proto as usize].p.nregs;
        self.push_frame(f, proto, nregs, fslot + 1, nargs)
    }

    /// Pops a frame; returns true when the outermost frame returned.
    #[inline(always)]
    fn do_return(&mut self, base: usize, v: Value) -> bool {
        if self.open.last().is_some_and(|&(i, _)| i as usize >= base) {
            self.close_upvals(base);
        }
        self.frames.pop();
        self.stack[base - 1] = v;
        self.frames.is_empty()
    }

    // ---- closure-threaded dispatch -------------------------------------

    fn exec_threaded(&mut self) -> R<Value> {
        let protos = self.protos.clone();
        loop {
            let fr = self.frames.last().expect("frame");
            let proto = &protos[fr.proto as usize];
            let code = &proto.threaded;
            let func = fr.func;
            let base = fr.base as usize;
            let mut pc = fr.pc as usize;
            loop {
                let t = &code[pc];
                pc += 1;
                match t(self, &mut pc, base, func)? {
                    Ctl::Next => {}
                    Ctl::Call { a, skip } => {
                        self.frames.last_mut().expect("frame").pc = (pc + skip) as u32;
                        let nargs = match proto.p.code[pc - 1] {
                            Op::Call { nargs, .. } => nargs,
                            _ => 1,
                        };
                        self.call(base + a as usize, nargs)?;
                        break;
                    }
                    Ctl::Ret { a, n } => {
                        let v = if n == 1 {
                            self.stack[base + a as usize]
                        } else {
                            Value::Nil
                        };
                        if self.do_return(base, v) {
                            return Ok(v);
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn thread_op(op: Op, k: &[Value]) -> Thunk {
    macro_rules! t {
        (|$vm:ident, $pc:ident, $base:ident, $func:ident| $body:expr) => {
            Box::new(
                move |$vm: &mut Vm, $pc: &mut usize, $base: usize, $func: FnRef| {
                    let _ = (&$pc, &$func, &$base);
                    let _ = &$vm;
                    $body
                },
            )
        };
    }
    macro_rules! gc {
        ($vm:ident, $base:ident) => {
            if $vm.heap.debt > 0 {
                let top = $vm.frames.last().map_or(0, |f| {
                    f.base as usize + $vm.protos[f.proto as usize].p.nregs as usize
                });
                $vm.gc_step(top);
            }
        };
    }
    match op {
        Op::Move { a, b } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = vm.stack[base + b as usize];
            Ok(Ctl::Next)
        }),
        Op::LoadI { a, i } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = Value::Int(i as i64);
            Ok(Ctl::Next)
        }),
        Op::LoadK { a, k: x } => {
            let v = k[x as usize];
            t!(|vm, pc, base, func| {
                vm.stack[base + a as usize] = v;
                Ok(Ctl::Next)
            })
        }
        Op::LoadNil { a } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = Value::Nil;
            Ok(Ctl::Next)
        }),
        Op::GetUpval { a, u } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = vm.get_upval(func, u);
            Ok(Ctl::Next)
        }),
        Op::SetUpval { a, u } => t!(|vm, pc, base, func| {
            let v = vm.stack[base + a as usize];
            vm.set_upval(func, u, v);
            Ok(Ctl::Next)
        }),
        Op::NewTable { a } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = Value::Table(vm.heap.new_table());
            gc!(vm, base);
            Ok(Ctl::Next)
        }),
        Op::GetTable { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] =
                vm.index(vm.stack[base + b as usize], vm.stack[base + c as usize])?;
            Ok(Ctl::Next)
        }),
        Op::SetTable { a, b, c } => t!(|vm, pc, base, func| {
            vm.set_index(
                vm.stack[base + a as usize],
                vm.stack[base + b as usize],
                vm.stack[base + c as usize],
            )?;
            gc!(vm, base);
            Ok(Ctl::Next)
        }),
        Op::GetField { a, b, k: x } => {
            let kv = k[x as usize];
            t!(|vm, pc, base, func| {
                vm.stack[base + a as usize] = vm.index(vm.stack[base + b as usize], kv)?;
                Ok(Ctl::Next)
            })
        }
        Op::SetField { a, k: x, c } => {
            let kv = k[x as usize];
            t!(|vm, pc, base, func| {
                vm.set_index(vm.stack[base + a as usize], kv, vm.stack[base + c as usize])?;
                gc!(vm, base);
                Ok(Ctl::Next)
            })
        }
        Op::Add { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = arith(
                vm.stack[base + b as usize],
                vm.stack[base + c as usize],
                i64::wrapping_add,
                |x, y| x + y,
            )?;
            Ok(Ctl::Next)
        }),
        Op::Sub { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = arith(
                vm.stack[base + b as usize],
                vm.stack[base + c as usize],
                i64::wrapping_sub,
                |x, y| x - y,
            )?;
            Ok(Ctl::Next)
        }),
        Op::Mul { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = arith(
                vm.stack[base + b as usize],
                vm.stack[base + c as usize],
                i64::wrapping_mul,
                |x, y| x * y,
            )?;
            Ok(Ctl::Next)
        }),
        Op::Mod { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] =
                imod(vm.stack[base + b as usize], vm.stack[base + c as usize])?;
            Ok(Ctl::Next)
        }),
        Op::AddI { a, b, i } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = addi(vm.stack[base + b as usize], i)?;
            Ok(Ctl::Next)
        }),
        Op::Concat { a, b, c } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] =
                vm.concat(vm.stack[base + b as usize], vm.stack[base + c as usize])?;
            gc!(vm, base);
            Ok(Ctl::Next)
        }),
        Op::Len { a, b } => t!(|vm, pc, base, func| {
            vm.stack[base + a as usize] = vm.len(vm.stack[base + b as usize])?;
            Ok(Ctl::Next)
        }),
        Op::Lt { a, b, k } => t!(|vm, pc, base, func| {
            if lt(vm.stack[base + a as usize], vm.stack[base + b as usize])? != k {
                *pc += 1;
            }
            Ok(Ctl::Next)
        }),
        Op::Le { a, b, k } => t!(|vm, pc, base, func| {
            if le(vm.stack[base + a as usize], vm.stack[base + b as usize])? != k {
                *pc += 1;
            }
            Ok(Ctl::Next)
        }),
        Op::LtI { a, i, k } => t!(|vm, pc, base, func| {
            if lt(vm.stack[base + a as usize], Value::Int(i as i64))? != k {
                *pc += 1;
            }
            Ok(Ctl::Next)
        }),
        Op::Jmp { to } => t!(|vm, pc, base, func| {
            *pc = to as usize;
            Ok(Ctl::Next)
        }),
        Op::ForPrep { a, exit } => t!(|vm, pc, base, func| {
            if for_prep(&mut vm.stack[base + a as usize..base + a as usize + 4])? {
                *pc = exit as usize;
            }
            Ok(Ctl::Next)
        }),
        Op::ForLoop { a, body } => t!(|vm, pc, base, func| {
            let s = &mut vm.stack[base + a as usize..base + a as usize + 4];
            if let (Value::Int(count), Value::Int(i), Value::Int(step)) = (s[1], s[0], s[2])
                && count > 0
            {
                s[1] = Value::Int(count - 1);
                let n = Value::Int(i.wrapping_add(step));
                s[0] = n;
                s[3] = n;
                *pc = body as usize;
            }
            Ok(Ctl::Next)
        }),
        Op::Closure { a, p } => t!(|vm, pc, base, func| {
            let protos = vm.protos.clone();
            let proto = &protos[vm.frames.last().expect("frame").proto as usize];
            vm.stack[base + a as usize] = vm.new_closure(proto, func, base, p);
            gc!(vm, base);
            Ok(Ctl::Next)
        }),
        Op::Close { a } => t!(|vm, pc, base, func| {
            vm.close_upvals(base + a as usize);
            Ok(Ctl::Next)
        }),
        Op::Call { a, .. } => t!(|vm, pc, base, func| Ok(Ctl::Call { a, skip: 0 })),
        Op::Return { a, n } => t!(|vm, pc, base, func| Ok(Ctl::Ret { a, n })),
        other => panic!("superinstruction {other:?} is not threaded"),
    }
}

pub fn clone_proto(p: &Proto) -> Proto {
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

#[inline(always)]
fn arith(a: Value, b: Value, fi: fn(i64, i64) -> i64, ff: fn(f64, f64) -> f64) -> R<Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(fi(x, y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(ff(x, y))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(ff(x as f64, y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(ff(x, y as f64))),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

#[inline(always)]
fn addi(a: Value, i: i16) -> R<Value> {
    match a {
        Value::Int(x) => Ok(Value::Int(x.wrapping_add(i as i64))),
        Value::Float(x) => Ok(Value::Float(x + i as f64)),
        _ => Err("attempt to perform arithmetic".into()),
    }
}

fn imod(a: Value, b: Value) -> R<Value> {
    match (a, b) {
        (Value::Int(_), Value::Int(0)) => Err("attempt to perform 'n%%0'".into()),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(lua_imod(x, y))),
        _ => Err("mod: only integers in the spike".into()),
    }
}

#[inline(always)]
fn lt(a: Value, b: Value) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x < y),
        (Value::Float(x), Value::Float(y)) => Ok(x < y),
        (Value::Int(x), Value::Float(y)) => Ok((x as f64) < y),
        (Value::Float(x), Value::Int(y)) => Ok(x < y as f64),
        _ => Err("attempt to compare".into()),
    }
}

#[inline(always)]
fn le(a: Value, b: Value) -> R<bool> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x <= y),
        (Value::Float(x), Value::Float(y)) => Ok(x <= y),
        (Value::Int(x), Value::Float(y)) => Ok((x as f64) <= y),
        (Value::Float(x), Value::Int(y)) => Ok(x <= y as f64),
        _ => Err("attempt to compare".into()),
    }
}

/// Integer loop prep as in Lua 5.4: stores the iteration count in R[a+1].
/// Returns true when the loop body is skipped.
fn for_prep(s: &mut [Value]) -> R<bool> {
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
