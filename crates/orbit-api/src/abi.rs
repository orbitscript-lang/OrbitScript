#![forbid(unsafe_code)]
//! The abi_stable projection: loads `liborbit_abi` next to the program (or
//! from ORBIT_LIB_DIR) and checks its layout, then wraps its trait objects
//! back into the safe API. No `unsafe` in this file.

use std::sync::Mutex;

use abi_stable::sabi_types::RMut;
use abi_stable::{
    library::RootModule,
    std_types::{RErr, ROk, RResult, RSlice, RString, RVec},
};
use orbit_abi_iface::{EngineBox, FVal, NativeCtx_TO, NativeExt, OrbitLib_Ref};
use orbit_api_types::{Ctx, NativeFn, Val};

fn to_f(v: &Val) -> FVal {
    match v {
        Val::Nil => FVal::Nil,
        Val::Bool(b) => FVal::Bool(*b),
        Val::Int(i) => FVal::Int(*i),
        Val::Float(f) => FVal::Float(*f),
        Val::Str(s) => FVal::Str(s.as_slice().into()),
        Val::Ref(r) => FVal::Ref(*r),
    }
}

fn from_f(v: FVal) -> Val {
    match v {
        FVal::Nil => Val::Nil,
        FVal::Bool(b) => Val::Bool(b),
        FVal::Int(i) => Val::Int(i),
        FVal::Float(f) => Val::Float(f),
        FVal::Str(s) => Val::Str(s.into_vec()),
        FVal::Ref(r) => Val::Ref(r),
    }
}

fn vals(r: RResult<RVec<FVal>, RString>) -> Result<Vec<Val>, String> {
    match r {
        ROk(v) => Ok(v.into_iter().map(from_f).collect()),
        RErr(m) => Err(m.into_string()),
    }
}

fn lib() -> Result<OrbitLib_Ref, String> {
    let dir = match std::env::var_os("ORBIT_LIB_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default(),
    };
    OrbitLib_Ref::load_from_directory(&dir).map_err(|e| e.to_string())
}

/// Natives the program registered; the library hands back the index.
static NATIVES: Mutex<Vec<NativeFn>> = Mutex::new(Vec::new());

pub struct State(EngineBox);

impl State {
    pub fn new(ffi: bool) -> Result<State, String> {
        Ok(State((lib()?.new_engine())(ffi)))
    }
    pub fn load(&mut self, src: &[u8], name: &str) -> Result<Val, String> {
        match self.0.load(src.into(), name.into()) {
            ROk(v) => Ok(from_f(v)),
            RErr(m) => Err(m.into_string()),
        }
    }
    pub fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let args: Vec<FVal> = args.iter().map(to_f).collect();
        vals(self.0.call(to_f(f), RSlice::from_slice(&args)))
    }
    pub fn set_global(&mut self, name: &str, v: &Val) {
        self.0.set_global(name.into(), to_f(v))
    }
    pub fn global(&mut self, name: &str) -> Val {
        from_f(self.0.global(name.into()))
    }
    pub fn new_table(&mut self) -> Val {
        from_f(self.0.new_table())
    }
    pub fn table_set(&mut self, t: &Val, k: &Val, v: &Val) {
        self.0.table_set(to_f(t), to_f(k), to_f(v))
    }
    pub fn release(&mut self, r: u32) {
        self.0.release(r)
    }
    pub fn tostring(&mut self, v: &Val) -> Vec<u8> {
        self.0.tostring(to_f(v)).into_vec()
    }
    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        let mut list = NATIVES.lock().unwrap_or_else(|e| e.into_inner());
        list.push(f);
        let i = list.len() - 1;
        drop(list);
        self.0.register(name.into(), NativeExt { func: tramp }, i)
    }
}

struct AbiCtx<'a, 'b>(NativeCtx_TO<'a, RMut<'b, ()>>);

impl Ctx for AbiCtx<'_, '_> {
    fn nargs(&self) -> usize {
        self.0.nargs()
    }
    fn arg(&mut self, i: usize) -> Val {
        from_f(self.0.arg(i))
    }
    fn push(&mut self, v: &Val) {
        self.0.push(to_f(v))
    }
    fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let args: Vec<FVal> = args.iter().map(to_f).collect();
        vals(self.0.call(to_f(f), RSlice::from_slice(&args)))
    }
}

extern "C" fn tramp<'a, 'b>(
    ctx: NativeCtx_TO<'a, RMut<'b, ()>>,
    data: usize,
) -> RResult<usize, RString> {
    let f = NATIVES.lock().unwrap_or_else(|e| e.into_inner())[data];
    match f(&mut AbiCtx(ctx)) {
        Ok(n) => ROk(n),
        Err(m) => RErr(m.into()),
    }
}
