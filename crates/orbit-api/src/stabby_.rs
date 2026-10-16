#![forbid(unsafe_code)]
//! The stabby projection: links `liborbit_stabby` at load time; the first
//! call checks the constructor's signature report against the library's,
//! then the trait objects are wrapped back into the safe API. No `unsafe`
//! in this file.

use orbit_api_types::{Ctx, NativeFn, Val};
use orbit_stabby_iface::{
    CtxMut, EngineBox, EngineDynMut, HostFn, NativeCtxDyn, NativeCtxDynMut, SVal, Vals,
};
use stabby::{
    boxed::Box as SBox, result::Result as SResult, slice::Slice, string::String as SString,
};

#[stabby::import(name = "orbit_stabby")]
unsafe extern "C" {
    pub fn orbit_stabby_new_engine(ffi: bool) -> orbit_stabby_iface::EngineBox;
}

fn to_s(v: &Val) -> SVal {
    match v {
        Val::Nil => SVal::Nil,
        Val::Bool(b) => SVal::Bool(*b),
        Val::Int(i) => SVal::Int(*i),
        Val::Float(f) => SVal::Float(*f),
        Val::Str(s) => SVal::Str(s.as_slice().into()),
        Val::Ref(r) => SVal::Ref(*r),
    }
}

fn from_s(v: SVal) -> Val {
    match v {
        SVal::Nil => Val::Nil,
        SVal::Bool(b) => Val::Bool(b),
        SVal::Int(i) => Val::Int(i),
        SVal::Float(f) => Val::Float(f),
        SVal::Str(s) => Val::Str(s.to_vec()),
        SVal::Ref(r) => Val::Ref(r),
    }
}

fn vals(r: Vals) -> Result<Vec<Val>, String> {
    r.match_owned(
        |v| Ok(v.into_iter().map(from_s).collect()),
        |m| Err(m.to_string()),
    )
}

pub struct State(EngineBox);

impl State {
    pub fn new(ffi: bool) -> Result<State, String> {
        Ok(State(orbit_stabby_new_engine(ffi)))
    }
    pub fn load(&mut self, src: &[u8], name: &str) -> Result<Val, String> {
        self.0
            .load(src.into(), name.into())
            .match_owned(|v| Ok(from_s(v)), |m| Err(m.to_string()))
    }
    pub fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let args: Vec<SVal> = args.iter().map(to_s).collect();
        vals(self.0.call(to_s(f), Slice::from(args.as_slice())))
    }
    pub fn set_global(&mut self, name: &str, v: &Val) {
        self.0.set_global(name.into(), to_s(v))
    }
    pub fn global(&mut self, name: &str) -> Val {
        from_s(self.0.global(name.into()))
    }
    pub fn new_table(&mut self) -> Val {
        from_s(self.0.new_table())
    }
    pub fn table_set(&mut self, t: &Val, k: &Val, v: &Val) {
        self.0.table_set(to_s(t), to_s(k), to_s(v))
    }
    pub fn release(&mut self, r: u32) {
        self.0.release(r)
    }
    pub fn tostring(&mut self, v: &Val) -> Vec<u8> {
        self.0.tostring(to_s(v)).to_vec()
    }
    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        self.0.register(name.into(), SBox::new(Native(f)).into())
    }
}

/// A program's native as the library holds it.
struct Native(NativeFn);

impl HostFn for Native {
    extern "C" fn run<'a>(&self, ctx: CtxMut<'a>) -> SResult<usize, SString> {
        match (self.0)(&mut StabbyCtx(ctx)) {
            Ok(n) => SResult::Ok(n),
            Err(m) => SResult::Err(m.as_str().into()),
        }
    }
}

struct StabbyCtx<'a>(CtxMut<'a>);

impl Ctx for StabbyCtx<'_> {
    fn nargs(&self) -> usize {
        self.0.nargs()
    }
    fn arg(&mut self, i: usize) -> Val {
        from_s(self.0.arg(i))
    }
    fn push(&mut self, v: &Val) {
        self.0.push(to_s(v))
    }
    fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let args: Vec<SVal> = args.iter().map(to_s).collect();
        vals(self.0.call(to_s(f), Slice::from(args.as_slice())))
    }
}
