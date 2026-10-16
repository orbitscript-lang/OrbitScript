//! The hand-written projection: `liborbit_table`'s function table, wrapped
//! back into the safe API. All of this file is the boundary.
#![allow(unsafe_code)]

use orbit_api_types::{Ctx, NativeFn, Val};
use orbit_table_iface::*;

unsafe extern "C" {
    fn orbit_api_table() -> *const ApiTable;
}

fn raw(v: &Val) -> RawVal {
    match v {
        Val::Nil => RawVal::NIL,
        Val::Bool(b) => RawVal {
            tag: BOOL,
            bits: *b as u64,
            ..RawVal::NIL
        },
        Val::Int(i) => RawVal {
            tag: INT,
            bits: *i as u64,
            ..RawVal::NIL
        },
        Val::Float(f) => RawVal {
            tag: FLOAT,
            bits: f.to_bits(),
            ..RawVal::NIL
        },
        Val::Str(s) => RawVal {
            tag: STR,
            bits: 0,
            ptr: s.as_ptr(),
            len: s.len(),
        },
        Val::Ref(r) => RawVal {
            tag: REF,
            bits: *r as u64,
            ..RawVal::NIL
        },
    }
}

/// # Safety
/// A string's `ptr`/`len` are readable now.
unsafe fn val(r: &RawVal) -> Val {
    match r.tag {
        BOOL => Val::Bool(r.bits != 0),
        INT => Val::Int(r.bits as i64),
        FLOAT => Val::Float(f64::from_bits(r.bits)),
        STR if r.len == 0 => Val::Str(Vec::new()),
        STR => Val::Str(unsafe { std::slice::from_raw_parts(r.ptr, r.len) }.to_vec()),
        REF => Val::Ref(r.bits as u32),
        _ => Val::Nil,
    }
}

fn text(v: Val) -> String {
    match v {
        Val::Str(s) => String::from_utf8_lossy(&s).into_owned(),
        _ => "error".into(),
    }
}

pub struct State {
    api: &'static ApiTable,
    p: StatePtr,
}

impl State {
    pub fn new(ffi: bool) -> Result<State, String> {
        // SAFETY: the library returns a pointer to a static table.
        let api = unsafe { &*orbit_api_table() };
        if api.version != VERSION {
            return Err(format!(
                "liborbit has API version {}, expected {VERSION}",
                api.version
            ));
        }
        let p = unsafe { (api.new_state)(ffi) };
        Ok(State { api, p })
    }

    fn one(&self, f: impl FnOnce(&mut RawVal)) -> Val {
        let mut r = RawVal::NIL;
        f(&mut r);
        // SAFETY: strings stay valid until the next call on this state.
        unsafe { val(&r) }
    }

    pub fn load(&mut self, src: &[u8], name: &str) -> Result<Val, String> {
        let mut r = RawVal::NIL;
        let st = unsafe {
            (self.api.load)(
                self.p,
                src.as_ptr(),
                src.len(),
                name.as_ptr(),
                name.len(),
                &mut r,
            )
        };
        let v = unsafe { val(&r) };
        if st == 0 { Ok(v) } else { Err(text(v)) }
    }

    pub fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let f = raw(f);
        let a: Vec<RawVal> = args.iter().map(raw).collect();
        let mut n = 0;
        let st = unsafe { (self.api.call)(self.p, &f, a.as_ptr(), a.len(), &mut n) };
        let out: Vec<Val> = (0..n)
            .map(|i| self.one(|r| unsafe { (self.api.result)(self.p, i, r) }))
            .collect();
        if st == 0 {
            Ok(out)
        } else {
            Err(text(out.into_iter().next().unwrap_or(Val::Nil)))
        }
    }

    pub fn set_global(&mut self, name: &str, v: &Val) {
        unsafe { (self.api.set_global)(self.p, name.as_ptr(), name.len(), &raw(v)) }
    }

    pub fn global(&mut self, name: &str) -> Val {
        self.one(|r| unsafe { (self.api.global)(self.p, name.as_ptr(), name.len(), r) })
    }

    pub fn new_table(&mut self) -> Val {
        self.one(|r| unsafe { (self.api.new_table)(self.p, r) })
    }

    pub fn table_set(&mut self, t: &Val, k: &Val, v: &Val) {
        unsafe { (self.api.table_set)(self.p, &raw(t), &raw(k), &raw(v)) }
    }

    pub fn release(&mut self, r: u32) {
        unsafe { (self.api.release)(self.p, r) }
    }

    pub fn tostring(&mut self, v: &Val) -> Vec<u8> {
        match self.one(|r| unsafe { (self.api.tostring)(self.p, &raw(v), r) }) {
            Val::Str(s) => s,
            _ => Vec::new(),
        }
    }

    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        unsafe { (self.api.register)(self.p, name.as_ptr(), name.len(), tramp, f as usize) }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe { (self.api.free_state)(self.p) }
    }
}

struct TableCtx {
    p: CtxPtr,
    ops: &'static CtxTable,
}

impl Ctx for TableCtx {
    fn nargs(&self) -> usize {
        unsafe { (self.ops.nargs)(self.p) }
    }
    fn arg(&mut self, i: usize) -> Val {
        let mut r = RawVal::NIL;
        unsafe {
            (self.ops.arg)(self.p, i, &mut r);
            val(&r)
        }
    }
    fn push(&mut self, v: &Val) {
        unsafe { (self.ops.push)(self.p, &raw(v)) }
    }
    fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let f = raw(f);
        let a: Vec<RawVal> = args.iter().map(raw).collect();
        let mut n = 0;
        let st = unsafe { (self.ops.call)(self.p, &f, a.as_ptr(), a.len(), &mut n) };
        let out: Vec<Val> = (0..n)
            .map(|i| {
                let mut r = RawVal::NIL;
                unsafe {
                    (self.ops.result)(self.p, i, &mut r);
                    val(&r)
                }
            })
            .collect();
        if st == 0 {
            Ok(out)
        } else {
            Err(text(out.into_iter().next().unwrap_or(Val::Nil)))
        }
    }
}

unsafe extern "C" fn tramp(p: CtxPtr, ops: *const CtxTable, data: usize) -> i64 {
    // SAFETY: `data` is the `NativeFn` `register` passed; `ops` is static.
    let f = unsafe { std::mem::transmute::<usize, NativeFn>(data) };
    let mut c = TableCtx {
        p,
        ops: unsafe { &*ops },
    };
    match f(&mut c) {
        Ok(n) => n as i64,
        Err(m) => {
            unsafe { (c.ops.set_error)(p, m.as_ptr(), m.len()) };
            -1
        }
    }
}
