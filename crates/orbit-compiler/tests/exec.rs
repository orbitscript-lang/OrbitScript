//! Semantic checks for the compiler: a deliberately naive bytecode interpreter (test only,
//! Rc tables, Rust recursion, no GC) runs compiled snippets, and their output is compared
//! with what the reference `lua5.5` printed for the same source (recorded inline).
//!
//! This is not the VM. It exists so compiler bugs show up without waiting on `orbit-vm`,
//! and it covers only what the snippets use.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use orbit_bytecode::{CaptureSource, Constant, MULTI, Op, UnlinkedChunk};
use orbit_compiler::{compile, disassemble};

#[derive(Clone)]
enum V {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<[u8]>),
    Table(Rc<RefCell<HashMap<Key, V>>>),
    Func(Rc<Closure>),
    Native(fn(&mut Interp, Vec<V>) -> Result<Vec<V>, String>),
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Bool(bool),
    Int(i64),
    Float(u64),
    Str(Rc<[u8]>),
    Ptr(usize),
}

struct Closure {
    proto: usize,
    caps: Vec<Rc<RefCell<V>>>,
}

type Cell = Rc<RefCell<V>>;

impl V {
    fn truthy(&self) -> bool {
        !matches!(self, V::Nil | V::Bool(false))
    }
    fn num(&self) -> Option<f64> {
        match self {
            V::Int(i) => Some(*i as f64),
            V::Float(x) => Some(*x),
            _ => None,
        }
    }
    fn key(&self) -> Result<Key, String> {
        Ok(match self {
            V::Nil => return Err("table index is nil".into()),
            V::Bool(b) => Key::Bool(*b),
            V::Int(i) => Key::Int(*i),
            V::Float(x) if x.fract() == 0.0 && x.abs() < 9.2e18 => Key::Int(*x as i64),
            V::Float(x) => Key::Float(x.to_bits()),
            V::Str(s) => Key::Str(s.clone()),
            V::Table(t) => Key::Ptr(Rc::as_ptr(t) as *const () as usize),
            V::Func(f) => Key::Ptr(Rc::as_ptr(f) as *const () as usize),
            V::Native(f) => Key::Ptr(*f as usize),
        })
    }
    fn show(&self) -> String {
        match self {
            V::Nil => "nil".into(),
            V::Bool(b) => b.to_string(),
            V::Int(i) => i.to_string(),
            V::Float(x) => {
                if x.fract() == 0.0 && x.abs() < 1e15 {
                    format!("{x:.1}")
                } else {
                    format!("{x}")
                }
            }
            V::Str(s) => String::from_utf8_lossy(s).into(),
            V::Table(_) => "table".into(),
            V::Func(_) | V::Native(_) => "function".into(),
        }
    }
}

fn eq(a: &V, b: &V) -> bool {
    match (a, b) {
        (V::Nil, V::Nil) => true,
        (V::Bool(x), V::Bool(y)) => x == y,
        (V::Str(x), V::Str(y)) => x == y,
        (V::Int(x), V::Int(y)) => x == y,
        (V::Int(_) | V::Float(_), V::Int(_) | V::Float(_)) => a.num() == b.num(),
        (V::Table(x), V::Table(y)) => Rc::ptr_eq(x, y),
        (V::Func(x), V::Func(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

struct Interp {
    chunk: UnlinkedChunk,
    out: String,
}

fn s(x: &str) -> V {
    V::Str(x.as_bytes().into())
}

impl Interp {
    fn konst(&self, proto: usize, k: u16) -> V {
        match &self.chunk.protos[proto].consts[k as usize] {
            Constant::Int(i) => V::Int(*i),
            Constant::Float(x) => V::Float(*x),
            Constant::Str(b) => V::Str(b.clone().into()),
        }
    }

    fn call(&mut self, f: V, args: Vec<V>) -> Result<Vec<V>, String> {
        match f {
            V::Func(c) => self.run(&c, args),
            V::Native(n) => n(self, args),
            other => Err(format!("attempt to call a {} value", other.show())),
        }
    }

    fn index(&self, t: &V, k: &V) -> Result<V, String> {
        match t {
            V::Table(t) => Ok(t.borrow().get(&k.key()?).cloned().unwrap_or(V::Nil)),
            _ => Err(format!("attempt to index a {} value", t.show())),
        }
    }

    fn set(&self, t: &V, k: &V, v: V) -> Result<(), String> {
        match t {
            V::Table(t) => {
                let key = k.key()?;
                if let V::Nil = v {
                    t.borrow_mut().remove(&key);
                } else {
                    t.borrow_mut().insert(key, v);
                }
                Ok(())
            }
            _ => Err(format!("attempt to index a {} value", t.show())),
        }
    }

    fn arith(&self, op: &Op, a: &V, b: &V) -> Result<V, String> {
        if let (V::Int(x), V::Int(y)) = (a, b) {
            let (x, y) = (*x, *y);
            return Ok(match op {
                Op::Add { .. } | Op::AddInt { .. } => V::Int(x.wrapping_add(y)),
                Op::Sub { .. } => V::Int(x.wrapping_sub(y)),
                Op::Mul { .. } => V::Int(x.wrapping_mul(y)),
                Op::IDiv { .. } => {
                    if y == 0 {
                        return Err("attempt to perform 'n//0'".into());
                    }
                    V::Int(x.div_euclid(y) - if y < 0 && x.rem_euclid(y) != 0 { 1 } else { 0 })
                }
                Op::Mod { .. } => {
                    if y == 0 {
                        return Err("attempt to perform 'n%0'".into());
                    }
                    let r = x % y;
                    V::Int(if r != 0 && (r ^ y) < 0 { r + y } else { r })
                }
                Op::Div { .. } => V::Float(x as f64 / y as f64),
                Op::Pow { .. } => V::Float((x as f64).powf(y as f64)),
                _ => unreachable!(),
            });
        }
        let (Some(x), Some(y)) = (a.num(), b.num()) else {
            return Err(format!(
                "attempt to perform arithmetic on {} and {}",
                a.show(),
                b.show()
            ));
        };
        Ok(V::Float(match op {
            Op::Add { .. } | Op::AddInt { .. } => x + y,
            Op::Sub { .. } => x - y,
            Op::Mul { .. } => x * y,
            Op::Div { .. } => x / y,
            Op::IDiv { .. } => (x / y).floor(),
            Op::Mod { .. } => {
                let m = x % y;
                if m != 0.0 && (m < 0.0) != (y < 0.0) {
                    m + y
                } else {
                    m
                }
            }
            Op::Pow { .. } => x.powf(y),
            _ => unreachable!(),
        }))
    }

    fn lt(a: &V, b: &V) -> Result<bool, String> {
        match (a, b) {
            (V::Int(x), V::Int(y)) => Ok(x < y),
            (V::Str(x), V::Str(y)) => Ok(x < y),
            _ => match (a.num(), b.num()) {
                (Some(x), Some(y)) => Ok(x < y),
                _ => Err(format!("attempt to compare {} with {}", a.show(), b.show())),
            },
        }
    }

    fn le(a: &V, b: &V) -> Result<bool, String> {
        match (a, b) {
            (V::Int(x), V::Int(y)) => Ok(x <= y),
            (V::Str(x), V::Str(y)) => Ok(x <= y),
            _ => match (a.num(), b.num()) {
                (Some(x), Some(y)) => Ok(x <= y),
                _ => Err(format!("attempt to compare {} with {}", a.show(), b.show())),
            },
        }
    }

    fn run(&mut self, c: &Rc<Closure>, mut args: Vec<V>) -> Result<Vec<V>, String> {
        let pi = c.proto;
        let p = self.chunk.protos[pi].clone();
        let np = p.nparams as usize;
        let varargs = if args.len() > np {
            args.split_off(np)
        } else {
            Vec::new()
        };
        let mut r: Vec<V> = vec![V::Nil; p.nregs as usize + 256];
        for (i, a) in args.into_iter().enumerate() {
            r[i] = a;
        }
        let mut cells: Vec<Cell> = (0..p.ncells)
            .map(|_| Rc::new(RefCell::new(V::Nil)))
            .collect();
        let mut top = 0usize;
        let mut pc = 0usize;
        let jump = |pc: &mut usize, off: i32| *pc = (*pc as i64 + off as i64) as usize;
        let range = |r: &Vec<V>, first: u8, count: u8, top: usize| -> Vec<V> {
            let end = if count == MULTI {
                top
            } else {
                first as usize + count as usize
            };
            r[first as usize..end].to_vec()
        };
        loop {
            let op = p.code[pc];
            pc += 1;
            match op {
                Op::Move { dst, src } => r[dst as usize] = r[src as usize].clone(),
                Op::LoadNil { dst, count } => {
                    (0..count).for_each(|i| r[(dst + i) as usize] = V::Nil)
                }
                Op::LoadBool { dst, value } => r[dst as usize] = V::Bool(value),
                Op::LoadInt { dst, value } => r[dst as usize] = V::Int(value as i64),
                Op::LoadConst { dst, k } => r[dst as usize] = self.konst(pi, k),
                Op::CellNew { cell, src } => {
                    cells[cell as usize] = Rc::new(RefCell::new(r[src as usize].clone()))
                }
                Op::CellGet { dst, cell } => {
                    r[dst as usize] = cells[cell as usize].borrow().clone()
                }
                Op::CellSet { cell, src } => {
                    *cells[cell as usize].borrow_mut() = r[src as usize].clone()
                }
                Op::CapGet { dst, cap } => r[dst as usize] = c.caps[cap as usize].borrow().clone(),
                Op::CapSelf { dst } => r[dst as usize] = V::Func(c.clone()),
                Op::CapSet { cap, src } => {
                    *c.caps[cap as usize].borrow_mut() = r[src as usize].clone()
                }
                Op::GetGlobal { dst, name, .. } => {
                    let env = c.caps[0].borrow().clone();
                    r[dst as usize] = self.index(&env, &self.konst(pi, name))?;
                }
                Op::SetGlobal { src, name, .. } => {
                    let env = c.caps[0].borrow().clone();
                    self.set(&env, &self.konst(pi, name), r[src as usize].clone())?;
                }
                Op::NewTable { dst, .. } => r[dst as usize] = V::Table(Rc::default()),
                Op::GetField { dst, obj, key, .. } => {
                    r[dst as usize] = self.index(&r[obj as usize], &self.konst(pi, key))?
                }
                Op::SetField { obj, key, src, .. } => self.set(
                    &r[obj as usize],
                    &self.konst(pi, key),
                    r[src as usize].clone(),
                )?,
                Op::GetIndex { dst, obj, key } => {
                    r[dst as usize] = self.index(&r[obj as usize], &r[key as usize])?
                }
                Op::SetIndex { obj, key, src } => {
                    self.set(&r[obj as usize], &r[key as usize], r[src as usize].clone())?
                }
                Op::SetList {
                    table,
                    first,
                    count,
                    start,
                } => {
                    for (i, v) in range(&r, first, count, top).into_iter().enumerate() {
                        self.set(&r[table as usize], &V::Int(start as i64 + i as i64), v)?;
                    }
                }
                Op::SelfCall { dst, obj, key } => {
                    let o = r[obj as usize].clone();
                    r[dst as usize + 1] = o.clone();
                    r[dst as usize] = self.index(&o, &self.konst(pi, key))?;
                }
                Op::Add { dst, a, b }
                | Op::Sub { dst, a, b }
                | Op::Mul { dst, a, b }
                | Op::Div { dst, a, b }
                | Op::IDiv { dst, a, b }
                | Op::Mod { dst, a, b }
                | Op::Pow { dst, a, b } => {
                    r[dst as usize] = self.arith(&op, &r[a as usize], &r[b as usize])?
                }
                Op::AddInt { dst, a, imm } => {
                    r[dst as usize] = self.arith(&op, &r[a as usize], &V::Int(imm as i64))?
                }
                Op::Neg { dst, src } => {
                    r[dst as usize] = match r[src as usize] {
                        V::Int(i) => V::Int(i.wrapping_neg()),
                        V::Float(x) => V::Float(-x),
                        _ => return Err("attempt to perform arithmetic".into()),
                    }
                }
                Op::Not { dst, src } => r[dst as usize] = V::Bool(!r[src as usize].truthy()),
                Op::Len { dst, src } => {
                    let n = match &r[src as usize] {
                        V::Str(s) => s.len() as i64,
                        V::Table(t) => {
                            let t = t.borrow();
                            let mut n = 0;
                            while t.contains_key(&Key::Int(n + 1)) {
                                n += 1;
                            }
                            n
                        }
                        _ => return Err("attempt to get length".into()),
                    };
                    r[dst as usize] = V::Int(n);
                }
                Op::Concat { dst, first, count } => {
                    let mut out = Vec::new();
                    for v in range(&r, first, count, top) {
                        match v {
                            V::Str(s) => out.extend_from_slice(&s),
                            V::Int(_) | V::Float(_) => out.extend_from_slice(v.show().as_bytes()),
                            _ => return Err("attempt to concatenate".into()),
                        }
                    }
                    r[dst as usize] = V::Str(out.into());
                }
                Op::Jump { off } => jump(&mut pc, off),
                Op::JumpIfTrue { src, off } => {
                    if r[src as usize].truthy() {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpIfFalse { src, off } => {
                    if !r[src as usize].truthy() {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpLt { a, b, off } => {
                    if Self::lt(&r[a as usize], &r[b as usize])? {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpNotLt { a, b, off } => {
                    if !Self::lt(&r[a as usize], &r[b as usize])? {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpLe { a, b, off } => {
                    if Self::le(&r[a as usize], &r[b as usize])? {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpNotLe { a, b, off } => {
                    if !Self::le(&r[a as usize], &r[b as usize])? {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpEq { a, b, off } => {
                    if eq(&r[a as usize], &r[b as usize]) {
                        jump(&mut pc, off)
                    }
                }
                Op::JumpNe { a, b, off } => {
                    if !eq(&r[a as usize], &r[b as usize]) {
                        jump(&mut pc, off)
                    }
                }
                Op::Call {
                    func,
                    nargs,
                    nresults,
                } => {
                    let args = range(&r, func + 1, nargs, top);
                    let mut res = self.call(r[func as usize].clone(), args)?;
                    if nresults == MULTI {
                        top = func as usize + res.len();
                    } else {
                        res.resize(nresults as usize, V::Nil);
                    }
                    for (i, v) in res.into_iter().enumerate() {
                        r[func as usize + i] = v;
                    }
                }
                Op::TailCall { func, nargs } => {
                    let args = range(&r, func + 1, nargs, top);
                    return self.call(r[func as usize].clone(), args);
                }
                Op::Return { first, count } => return Ok(range(&r, first, count, top)),
                Op::Return0 => return Ok(Vec::new()),
                Op::Return1 { src } => return Ok(vec![r[src as usize].clone()]),
                Op::Vararg { dst, count } => {
                    let mut v = varargs.clone();
                    if count == MULTI {
                        top = dst as usize + v.len();
                    } else {
                        v.resize(count as usize, V::Nil);
                    }
                    for (i, x) in v.into_iter().enumerate() {
                        r[dst as usize + i] = x;
                    }
                }
                Op::ForPrep { base, exit } => {
                    let b = base as usize;
                    match (&r[b], &r[b + 1], &r[b + 2]) {
                        (V::Int(start), limit, V::Int(step)) => {
                            let (start, step) = (*start, *step);
                            if step == 0 {
                                return Err("'for' step is zero".into());
                            }
                            let limit = match limit {
                                V::Int(l) => *l,
                                V::Float(l) if step > 0 => l.floor().min(i64::MAX as f64) as i64,
                                V::Float(l) => l.ceil().max(i64::MIN as f64) as i64,
                                _ => return Err("'for' limit must be a number".into()),
                            };
                            if (step > 0 && start > limit) || (step < 0 && start < limit) {
                                jump(&mut pc, exit);
                                continue;
                            }
                            let count = if step > 0 {
                                (limit as u64).wrapping_sub(start as u64) / step as u64
                            } else {
                                (start as u64).wrapping_sub(limit as u64)
                                    / (step as u64).wrapping_neg()
                            };
                            r[b + 1] = V::Int(count as i64);
                            r[b + 3] = V::Int(start);
                        }
                        (start, limit, step) => {
                            let (Some(s), Some(l), Some(st)) =
                                (start.num(), limit.num(), step.num())
                            else {
                                return Err("'for' initial value must be a number".into());
                            };
                            if st == 0.0 {
                                return Err("'for' step is zero".into());
                            }
                            if (st > 0.0 && s > l) || (st < 0.0 && s < l) {
                                jump(&mut pc, exit);
                                continue;
                            }
                            r[b] = V::Float(s);
                            r[b + 1] = V::Float(l);
                            r[b + 2] = V::Float(st);
                            r[b + 3] = V::Float(s);
                        }
                    }
                }
                Op::ForLoop { base, back } => {
                    let b = base as usize;
                    match (&r[b], &r[b + 1], &r[b + 2]) {
                        (V::Int(cur), V::Int(count), V::Int(step)) => {
                            if *count as u64 > 0 {
                                let next = cur.wrapping_add(*step);
                                r[b + 1] = V::Int((*count as u64 - 1) as i64);
                                r[b] = V::Int(next);
                                r[b + 3] = V::Int(next);
                                jump(&mut pc, back);
                            }
                        }
                        (V::Float(cur), V::Float(l), V::Float(st)) => {
                            let next = cur + st;
                            if (*st > 0.0 && next <= *l) || (*st < 0.0 && next >= *l) {
                                r[b] = V::Float(next);
                                r[b + 3] = V::Float(next);
                                jump(&mut pc, back);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                Op::GenericCall { base, nvars } => {
                    let b = base as usize;
                    let mut res =
                        self.call(r[b].clone(), vec![r[b + 1].clone(), r[b + 2].clone()])?;
                    res.resize(nvars as usize, V::Nil);
                    for (i, v) in res.into_iter().enumerate() {
                        r[b + 4 + i] = v;
                    }
                }
                Op::GenericLoop { base, back } => {
                    let b = base as usize;
                    if !matches!(r[b + 4], V::Nil) {
                        r[b + 2] = r[b + 4].clone();
                        jump(&mut pc, back);
                    }
                }
                Op::Closure { dst, proto } => {
                    let child = p.children[proto as usize] as usize;
                    let caps = self.chunk.protos[child]
                        .captures
                        .iter()
                        .map(|s| match *s {
                            CaptureSource::Cell(i) => cells[i as usize].clone(),
                            CaptureSource::Capture(i) => c.caps[i as usize].clone(),
                            CaptureSource::Env => unreachable!(),
                        })
                        .collect();
                    r[dst as usize] = V::Func(Rc::new(Closure { proto: child, caps }));
                }
            }
        }
    }
}

fn print(it: &mut Interp, args: Vec<V>) -> Result<Vec<V>, String> {
    let line: Vec<String> = args.iter().map(V::show).collect();
    it.out.push_str(&line.join("\t"));
    it.out.push('\n');
    Ok(vec![])
}

fn select(_: &mut Interp, args: Vec<V>) -> Result<Vec<V>, String> {
    match &args[0] {
        V::Str(s) if &**s == b"#" => Ok(vec![V::Int(args.len() as i64 - 1)]),
        &V::Int(n) => Ok(args.into_iter().skip(n as usize).collect()),
        _ => Err("bad select".into()),
    }
}

fn ipairs_step(it: &mut Interp, args: Vec<V>) -> Result<Vec<V>, String> {
    let V::Int(i) = args[1] else { unreachable!() };
    let v = it.index(&args[0], &V::Int(i + 1))?;
    Ok(if let V::Nil = v {
        vec![V::Nil]
    } else {
        vec![V::Int(i + 1), v]
    })
}

fn ipairs(_: &mut Interp, args: Vec<V>) -> Result<Vec<V>, String> {
    Ok(vec![V::Native(ipairs_step), args[0].clone(), V::Int(0)])
}

fn run(src: &str) -> String {
    let hir =
        orbit_lua_syntax::to_hir("t.lua", src.as_bytes(), &Default::default()).expect("front end");
    let chunk = compile(&hir).expect("compile");
    let listing = disassemble(&chunk);
    let globals: Rc<RefCell<HashMap<Key, V>>> = Rc::default();
    for (name, f) in [
        ("print", print as fn(&mut Interp, Vec<V>) -> _),
        ("select", select),
        ("ipairs", ipairs),
    ] {
        globals
            .borrow_mut()
            .insert(s(name).key().unwrap(), V::Native(f));
    }
    let mut it = Interp {
        chunk,
        out: String::new(),
    };
    let env = Rc::new(RefCell::new(V::Table(globals)));
    let caps = it.chunk.protos[0]
        .captures
        .iter()
        .map(|_| env.clone())
        .collect();
    let main = Rc::new(Closure { proto: 0, caps });
    if let Err(e) = it.run(&main, vec![]) {
        it.out.push_str(&format!("error: {e}\n"));
    }
    if std::env::var_os("SHOW_BYTECODE").is_some() {
        eprintln!("{listing}");
    }
    it.out
}

/// Each case's expected output is what `lua5.5` printed for the same source.
fn check(src: &str, expected: &str) {
    let got = run(src);
    if got != expected {
        let hir = orbit_lua_syntax::to_hir("t.lua", src.as_bytes(), &Default::default()).unwrap();
        panic!(
            "source:\n{src}\nexpected:\n{expected}\ngot:\n{got}\n{}",
            disassemble(&compile(&hir).unwrap())
        );
    }
}

#[test]
fn arithmetic_and_locals() {
    check(
        "local a, b = 7, 2
         print(a + b, a - b, a * b, a // b, a % b, -a // b, -a % b, a - 0, 3 - 5)
         local c = a
         c = c + 1
         print(a, c, 2^10, 7 // 2.0)",
        "9\t5\t14\t3\t1\t-4\t1\t7\t-2\n7\t8\t1024.0\t3.0\n",
    );
}

#[test]
fn assignment_order() {
    check(
        "local a, b = 1, 2
         a, b = b, a
         print(a, b)
         local i, t = 1, {}
         i, t[i] = i + 1, 20
         print(i, t[1], t[2])
         local x, y, z = (function() return 1, 2, 3 end)()
         print(x, y, z)
         x, y = 10
         print(x, y)
         local q = 1
         q = {q, q + 1}
         print(q[1], q[2])",
        "2\t1\n2\t20\tnil\n1\t2\t3\n10\tnil\n1\t2\n",
    );
}

#[test]
fn closures_and_fresh_bindings() {
    check(
        "local fs = {}
         for i = 1, 3 do fs[i] = function() return i end end
         print(fs[1](), fs[2](), fs[3]())
         local gs = {}
         local j = 1
         while j <= 3 do local k = j * 10; gs[j] = function() k = k + 1; return k end; j = j + 1 end
         print(gs[1](), gs[1](), gs[2](), gs[3]())
         local function counter()
           local n = 0
           return function() n = n + 1; return n end, function() return n end
         end
         local inc, get = counter()
         inc(); inc()
         print(get())
         local function fact(n) if n <= 1 then return 1 end return n * fact(n - 1) end
         print(fact(10))
         local x = 1
         local function outer() return function() return function() x = x + 1; return x end end end
         print(outer()()(), x)",
        "1\t2\t3\n11\t12\t21\t31\n2\n3628800\n2\t2\n",
    );
}

#[test]
fn local_functions_refer_to_themselves() {
    check(
        "local fs = {}
         for i = 1, 2 do local function k() return k, i end fs[i] = k end
         local a, i = fs[2]()
         print(a == fs[2], i, a == fs[1])
         local function g(n) if n == 0 then return 'base' end g = function() return 'new' end return g(n - 1) end
         print(g(3), g(3))
         local function h(n)
           local function set() h = function() return 'h2' end end
           if n == 1 then set() end
           if n == 0 then return 'base' end
           return h(n - 1)
         end
         print(h(2), h(5))",
        "true\t2\tfalse\nnew\tnew\nh2\th2\n",
    );
}

#[test]
fn control_flow() {
    check(
        "local s = 0
         for i = 10, 1, -3 do s = s + i end
         print(s)
         for i = 1, 3 do for j = 1, 3 do if j == 2 then break end; s = s + j end end
         print(s)
         local n = 0
         repeat local m = n; n = n + 1 until m >= 3
         print(n)
         for x = 1.0, 2.0, 0.5 do print(x) end
         for _, v in ipairs({5, 6, 7}) do if v == 7 then break end print(v) end
         local r = 1
         if r > 2 then print('a') elseif r > 0 and not (r == 2) then print('b') else print('c') end
         print(1 < 2, 2 <= 1, 'a' < 'b', 1 == 1.0, 1 ~= 2, nil == false)
         print(nil or 'd', false and 1, 1 and 2, nil and nil or 3)
         local mm = true
         for i = mm and 1 or 2, 1 do print(i) end
         local k = 0
         while true do k = k + 1; if k > 4 then break end end
         print(k)",
        "22\n25\n4\n1.0\n1.5\n2.0\n5\n6\nb\ntrue\tfalse\ttrue\ttrue\ttrue\tfalse\nd\tfalse\t2\t3\n1\n5\n",
    );
}

#[test]
fn tables_varargs_and_methods() {
    check(
        "local function f(...) return select('#', ...), ... end
         print(f(1, nil, 3))
         print((f(1, 2)))
         local t = {f(1, 2)}
         print(#t, t[1], t[2], t[3])
         local u = {f(1, 2), 'x'}
         print(#u, u[1], u[2])
         local obj = {n = 5}
         function obj:get(d) return self.n + d end
         print(obj:get(1), obj.get(obj, 2))
         local big = {}
         for i = 1, 120 do big[i] = i end
         local c = {0, 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53}
         print(#big, #c, c[54])
         local k <const> = 'key'
         local m = {[k] = 1, [1 + 1] = 2, x = 3}
         print(m.key, m[2], m.x)
         local function g(a, ...) local b, c = ... ; return a, b, c end
         print(g(1), g(1, 2, 3, 4))
         print('a' .. 'b' .. 1 .. 2.5)",
        "3\t1\tnil\t3\n2\n3\t2\t1\t2\n2\t2\tx\n6\t7\n120\t54\t53\n1\t2\t3\n1\t1\t2\t3\nab12.5\n",
    );
}

#[test]
fn tail_calls_run_in_constant_space_for_the_compiler() {
    // The naive interpreter recurses on calls, so only the shape is checked here.
    let hir = orbit_lua_syntax::to_hir(
        "t.lua",
        b"local function loop(n, acc) if n == 0 then return acc end return loop(n - 1, acc + 1) end return loop(10, 0)",
        &Default::default(),
    )
    .unwrap();
    let chunk = compile(&hir).unwrap();
    assert!(
        chunk.protos[1]
            .code
            .iter()
            .any(|op| matches!(op, Op::TailCall { .. }))
    );
    let main = &chunk.protos[0].code;
    assert!(matches!(main[main.len() - 2], Op::TailCall { .. }));
    assert!(matches!(main.last(), Some(Op::Return { count: MULTI, .. })));
}
