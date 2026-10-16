//! SPIKE: liborbit's side of the hand-written function table. All of this
//! file is the boundary, so all of it may use `unsafe`.
#![allow(unsafe_code)]

use orbit_api_core::orbit::{LuaError, NativeCtx, NativeReturn};
use orbit_api_core::{Ctx as CoreCtx, CtxApi, Engine, Val};
use orbit_table_iface::*;

struct St {
    e: Engine,
    /// Results of the last operation; strings handed out point into these.
    out: Vec<Val>,
}

fn to_raw(v: &Val) -> RawVal {
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
/// A string's `ptr`/`len` are readable.
unsafe fn from_raw(r: &RawVal) -> Val {
    match r.tag {
        BOOL => Val::Bool(r.bits != 0),
        INT => Val::Int(r.bits as i64),
        FLOAT => Val::Float(f64::from_bits(r.bits)),
        STR => Val::Str(unsafe { bytes(r.ptr, r.len) }.to_vec()),
        REF => Val::Ref(r.bits as u32),
        _ => Val::Nil,
    }
}

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}

unsafe fn st<'a>(p: StatePtr) -> &'a mut St {
    unsafe { &mut *(p as *mut St) }
}

unsafe fn args_of(a: *const RawVal, n: usize) -> Vec<Val> {
    let a = if n == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(a, n) }
    };
    a.iter().map(|r| unsafe { from_raw(r) }).collect()
}

unsafe extern "C" fn new_state(ffi: bool) -> StatePtr {
    Box::into_raw(Box::new(St {
        e: Engine::new(ffi),
        out: Vec::new(),
    })) as StatePtr
}

unsafe extern "C" fn free_state(p: StatePtr) {
    drop(unsafe { Box::from_raw(p as *mut St) });
}

unsafe extern "C" fn load(
    p: StatePtr,
    src: *const u8,
    n: usize,
    name: *const u8,
    nn: usize,
    out: *mut RawVal,
) -> i32 {
    let s = unsafe { st(p) };
    let name = String::from_utf8_lossy(unsafe { bytes(name, nn) }).into_owned();
    let (v, status) = match s.e.load(unsafe { bytes(src, n) }, &name) {
        Ok(v) => (v, 0),
        Err(m) => (Val::Str(m.into_bytes()), 1),
    };
    s.out = vec![v];
    unsafe { *out = to_raw(&s.out[0]) };
    status
}

unsafe extern "C" fn call(
    p: StatePtr,
    f: *const RawVal,
    a: *const RawVal,
    n: usize,
    nres: *mut usize,
) -> i32 {
    let s = unsafe { st(p) };
    let f = unsafe { from_raw(&*f) };
    let args = unsafe { args_of(a, n) };
    let (out, status) = match s.e.call(&f, &args) {
        Ok(v) => (v, 0),
        Err(m) => (vec![Val::Str(m.into_bytes())], 1),
    };
    s.out = out;
    unsafe { *nres = s.out.len() };
    status
}

unsafe extern "C" fn result(p: StatePtr, i: usize, out: *mut RawVal) {
    let s = unsafe { st(p) };
    unsafe { *out = s.out.get(i).map_or(RawVal::NIL, to_raw) };
}

unsafe extern "C" fn set_global(p: StatePtr, name: *const u8, nn: usize, v: *const RawVal) {
    let s = unsafe { st(p) };
    let name = String::from_utf8_lossy(unsafe { bytes(name, nn) });
    s.e.set_global(&name, &unsafe { from_raw(&*v) });
}

unsafe extern "C" fn global(p: StatePtr, name: *const u8, nn: usize, out: *mut RawVal) {
    let s = unsafe { st(p) };
    let name = String::from_utf8_lossy(unsafe { bytes(name, nn) });
    s.out = vec![s.e.global(&name)];
    unsafe { *out = to_raw(&s.out[0]) };
}

unsafe extern "C" fn new_table(p: StatePtr, out: *mut RawVal) {
    let s = unsafe { st(p) };
    s.out = vec![s.e.new_table()];
    unsafe { *out = to_raw(&s.out[0]) };
}

unsafe extern "C" fn table_set(p: StatePtr, t: *const RawVal, k: *const RawVal, v: *const RawVal) {
    let s = unsafe { st(p) };
    let (t, k, v) = unsafe { (from_raw(&*t), from_raw(&*k), from_raw(&*v)) };
    s.e.table_set(&t, &k, &v);
}

unsafe extern "C" fn release(p: StatePtr, r: u32) {
    unsafe { st(p) }.e.release(r);
}

unsafe extern "C" fn tostring(p: StatePtr, v: *const RawVal, out: *mut RawVal) {
    let s = unsafe { st(p) };
    let v = unsafe { from_raw(&*v) };
    s.out = vec![Val::Str(s.e.tostring(&v))];
    unsafe { *out = to_raw(&s.out[0]) };
}

unsafe extern "C" fn register(
    p: StatePtr,
    name: *const u8,
    nn: usize,
    f: NativeTramp,
    data: usize,
) {
    let s = unsafe { st(p) };
    let name: &'static str = Box::leak(
        String::from_utf8_lossy(unsafe { bytes(name, nn) })
            .into_owned()
            .into_boxed_str(),
    );
    s.e.register_raw(name, run_native, [f as usize as u64, data as u64]);
}

static API: ApiTable = ApiTable {
    version: VERSION,
    new_state,
    free_state,
    load,
    call,
    result,
    set_global,
    global,
    new_table,
    table_set,
    release,
    tostring,
    register,
};

/// The library's one export.
#[unsafe(no_mangle)]
pub extern "C" fn orbit_api_table() -> *const ApiTable {
    &API
}

// ---- natives ----------------------------------------------------------------

struct C<'a, 'b, 'c> {
    core: &'a mut CoreCtx<'b, 'c>,
    out: Vec<Val>,
    err: Option<String>,
}

unsafe fn cx<'a>(p: CtxPtr) -> &'a mut C<'a, 'a, 'a> {
    unsafe { &mut *(p as *mut C<'a, 'a, 'a>) }
}

unsafe extern "C" fn c_nargs(p: CtxPtr) -> usize {
    unsafe { cx(p) }.core.nargs()
}
unsafe extern "C" fn c_arg(p: CtxPtr, i: usize, out: *mut RawVal) {
    let c = unsafe { cx(p) };
    c.out = vec![c.core.arg(i)];
    unsafe { *out = to_raw(&c.out[0]) };
}
unsafe extern "C" fn c_push(p: CtxPtr, v: *const RawVal) {
    let c = unsafe { cx(p) };
    c.core.push(&unsafe { from_raw(&*v) });
}
unsafe extern "C" fn c_call(
    p: CtxPtr,
    f: *const RawVal,
    a: *const RawVal,
    n: usize,
    nres: *mut usize,
) -> i32 {
    let c = unsafe { cx(p) };
    let f = unsafe { from_raw(&*f) };
    let args = unsafe { args_of(a, n) };
    let (out, status) = match c.core.call(&f, &args) {
        Ok(v) => (v, 0),
        Err(m) => (vec![Val::Str(m.into_bytes())], 1),
    };
    c.out = out;
    unsafe { *nres = c.out.len() };
    status
}
unsafe extern "C" fn c_result(p: CtxPtr, i: usize, out: *mut RawVal) {
    let c = unsafe { cx(p) };
    unsafe { *out = c.out.get(i).map_or(RawVal::NIL, to_raw) };
}
unsafe extern "C" fn c_set_error(p: CtxPtr, m: *const u8, n: usize) {
    let c = unsafe { cx(p) };
    c.err = Some(String::from_utf8_lossy(unsafe { bytes(m, n) }).into_owned());
}

static CTX: CtxTable = CtxTable {
    nargs: c_nargs,
    arg: c_arg,
    push: c_push,
    call: c_call,
    result: c_result,
    set_error: c_set_error,
};

fn run_native(nc: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let [f, data] = nc.native_data();
    // SAFETY: stored by `register` from a `NativeTramp`.
    let f = unsafe { std::mem::transmute::<usize, NativeTramp>(f as usize) };
    let mut core = CoreCtx::new(nc);
    let mut c = C {
        core: &mut core,
        out: Vec::new(),
        err: None,
    };
    // SAFETY: `c` lives across the call; the program only uses it through `CTX`.
    let n = unsafe { f(&mut c as *mut C as CtxPtr, &CTX, data as usize) };
    let err = c.err.take();
    if n < 0 {
        core.finish(Err(err.unwrap_or_else(|| "native failed".into())))
    } else {
        core.finish(Ok(n as usize))
    }
}
