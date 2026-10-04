//! A small object model for the tests, shaped like Lua's: leaf strings that
//! are never weak, and tables with a strong part plus a map that can be
//! strong, weak-keyed (ephemeron), weak-valued or all weak.

#![allow(dead_code)]

use orbit_gc::{AnyGc, Gc, Heap, Liveness, Marker, Trace, WeakMode, space};

#[derive(Debug)]
pub struct Text(pub String);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ref {
    Obj(Gc<Obj>),
    Text(Gc<Text>),
}

#[derive(Debug, Default)]
pub struct Obj {
    pub id: u64,
    pub strong: Vec<Ref>,
    pub map: Vec<(Ref, Ref)>,
    pub mode: Option<WeakMode>,
}

space! {
    pub struct TestSpace {
        texts: Text,
        objs: Obj,
    }
    // Small pages, so tests release and reallocate pages often.
    const PAGE_SLOTS: usize = 16;
}

pub type TestHeap = Heap<TestSpace>;

impl Ref {
    pub fn mark(self, m: &mut Marker<'_, TestSpace>) {
        match self {
            Ref::Obj(o) => m.mark(o),
            Ref::Text(t) => m.mark(t),
        }
    }

    fn erase(self, m: &Marker<'_, TestSpace>) -> AnyGc {
        match self {
            Ref::Obj(o) => m.erase(o),
            Ref::Text(t) => m.erase(t),
        }
    }

    fn is_live(self, live: &Liveness<'_, TestSpace>) -> bool {
        match self {
            Ref::Obj(o) => live.is_live(o),
            Ref::Text(_) => true,
        }
    }

    pub fn obj(self) -> Gc<Obj> {
        match self {
            Ref::Obj(o) => o,
            Ref::Text(_) => panic!("not an object"),
        }
    }
}

impl Trace<TestSpace> for Text {
    const LEAF: bool = true;
    fn trace(&self, _: &mut Marker<'_, TestSpace>) {}
    fn extra_bytes(&self) -> usize {
        self.0.capacity()
    }
}

impl Trace<TestSpace> for Obj {
    fn trace(&self, m: &mut Marker<'_, TestSpace>) {
        for r in &self.strong {
            r.mark(m);
        }
        let Some(mode) = self.mode else {
            for &(k, v) in &self.map {
                k.mark(m);
                v.mark(m);
            }
            return;
        };
        for &(k, v) in &self.map {
            // Strings are values, never weak. A strong half is marked, a
            // weak half is left alone, and an ephemeron value lives as long
            // as its key.
            let k_weak = mode.keys && matches!(k, Ref::Obj(_));
            let v_weak = mode.values && matches!(v, Ref::Obj(_));
            if !k_weak {
                k.mark(m);
            }
            if !v_weak {
                if k_weak && !mode.values {
                    let (ka, va) = (k.erase(m), v.erase(m));
                    m.ephemeron(ka, va);
                } else {
                    v.mark(m);
                }
            }
        }
        m.weak(mode);
    }

    fn extra_bytes(&self) -> usize {
        self.strong.capacity() * size_of::<Ref>() + self.map.capacity() * size_of::<(Ref, Ref)>()
    }

    fn clear_weak(&mut self, mode: WeakMode, live: &Liveness<'_, TestSpace>) {
        self.map
            .retain(|&(k, v)| (!mode.keys || k.is_live(live)) && (!mode.values || v.is_live(live)));
    }
}

/// Roots held by a test: a plain list, like a VM stack.
#[derive(Debug, Default)]
pub struct Stack(pub Vec<Ref>);

impl orbit_gc::Roots<TestSpace> for Stack {
    fn trace(&self, m: &mut Marker<'_, TestSpace>) {
        for r in &self.0 {
            r.mark(m);
        }
    }
}

pub fn obj(h: &mut TestHeap, id: u64) -> Gc<Obj> {
    h.alloc(Obj {
        id,
        ..Obj::default()
    })
}

pub fn weak_obj(h: &mut TestHeap, mode: WeakMode) -> Gc<Obj> {
    h.alloc(Obj {
        mode: Some(mode),
        ..Obj::default()
    })
}

pub fn text(h: &mut TestHeap, s: &str) -> Gc<Text> {
    h.alloc(Text(s.to_owned()))
}

pub fn link(h: &mut TestHeap, from: Gc<Obj>, to: Ref) {
    h.update(from, |o| o.strong.push(to));
}

pub fn put(h: &mut TestHeap, t: Gc<Obj>, k: Ref, v: Ref) {
    h.update(t, |o| o.map.push((k, v)));
}

pub fn lookup(h: &TestHeap, t: Gc<Obj>, k: Ref) -> Option<Ref> {
    h.get(t).map.iter().find(|e| e.0 == k).map(|e| e.1)
}

/// A deterministic xorshift generator, so tests need no dependencies.
#[derive(Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
