//! SPIKE: liborbit's side of the abi_stable projection. No `unsafe` here:
//! abi_stable's macros generate the boundary.

use abi_stable::{
    export_root_module,
    prefix_type::PrefixTypeTrait,
    sabi_extern_fn,
    sabi_trait::TD_Opaque,
    sabi_types::RMut,
    std_types::{RBox, RErr, ROk, RResult, RSlice, RStr, RString, RVec},
};
use orbit_abi_iface::{
    Engine, Engine_TO, EngineBox, FVal, NativeCtx, NativeCtx_TO, NativeExt, OrbitLib, OrbitLib_Ref,
};
use orbit_api_core::orbit::{LuaError, NativeCtx as VmCtx, NativeReturn};
use orbit_api_core::{Ctx as CoreCtx, CtxApi, Val};

fn to_f(v: Val) -> FVal {
    match v {
        Val::Nil => FVal::Nil,
        Val::Bool(b) => FVal::Bool(b),
        Val::Int(i) => FVal::Int(i),
        Val::Float(f) => FVal::Float(f),
        Val::Str(s) => FVal::Str(s.into()),
        Val::Ref(r) => FVal::Ref(r),
    }
}

fn from_f(v: &FVal) -> Val {
    match v {
        FVal::Nil => Val::Nil,
        FVal::Bool(b) => Val::Bool(*b),
        FVal::Int(i) => Val::Int(*i),
        FVal::Float(f) => Val::Float(*f),
        FVal::Str(s) => Val::Str(s.to_vec()),
        FVal::Ref(r) => Val::Ref(*r),
    }
}

fn res(r: Result<Vec<Val>, String>) -> RResult<RVec<FVal>, RString> {
    match r {
        Ok(v) => ROk(v.into_iter().map(to_f).collect()),
        Err(m) => RErr(m.into()),
    }
}

struct Eng(orbit_api_core::Engine);

/// Natives registered through this library: (function, the program's data).
#[derive(Default)]
struct Natives(Vec<(NativeExt, usize)>);

impl Engine for Eng {
    fn load(&mut self, src: RSlice<'_, u8>, name: RStr<'_>) -> RResult<FVal, RString> {
        match self.0.load(&src, &name) {
            Ok(v) => ROk(to_f(v)),
            Err(m) => RErr(m.into()),
        }
    }
    fn call(&mut self, f: FVal, args: RSlice<'_, FVal>) -> RResult<RVec<FVal>, RString> {
        let args: Vec<Val> = args.iter().map(from_f).collect();
        res(self.0.call(&from_f(&f), &args))
    }
    fn set_global(&mut self, name: RStr<'_>, v: FVal) {
        self.0.set_global(&name, &from_f(&v))
    }
    fn global(&mut self, name: RStr<'_>) -> FVal {
        to_f(self.0.global(&name))
    }
    fn new_table(&mut self) -> FVal {
        to_f(self.0.new_table())
    }
    fn table_set(&mut self, t: FVal, k: FVal, v: FVal) {
        self.0.table_set(&from_f(&t), &from_f(&k), &from_f(&v))
    }
    fn release(&mut self, r: u32) {
        self.0.release(r)
    }
    fn tostring(&mut self, v: FVal) -> RVec<u8> {
        self.0.tostring(&from_f(&v)).into()
    }
    fn register(&mut self, name: RStr<'_>, f: NativeExt, data: usize) {
        let list = &mut self.0.ext::<Natives>().0;
        list.push((f, data));
        let i = list.len() as u64 - 1;
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        self.0.register_raw(name, run_native, [i, 0]);
    }
}

struct C<'a, 'b, 'c>(&'a mut CoreCtx<'b, 'c>);

impl NativeCtx for C<'_, '_, '_> {
    fn nargs(&self) -> usize {
        self.0.nargs()
    }
    fn arg(&mut self, i: usize) -> FVal {
        to_f(self.0.arg(i))
    }
    fn push(&mut self, v: FVal) {
        self.0.push(&from_f(&v))
    }
    fn call(&mut self, f: FVal, args: RSlice<'_, FVal>) -> RResult<RVec<FVal>, RString> {
        let args: Vec<Val> = args.iter().map(from_f).collect();
        res(self.0.call(&from_f(&f), &args))
    }
}

fn run_native(nc: &mut VmCtx<'_>) -> Result<NativeReturn, LuaError> {
    let i = nc.native_data()[0] as usize;
    let (f, data) = nc
        .vm
        .rt
        .ext
        .get::<Natives>()
        .map(|n| n.0[i])
        .expect("registered");
    let mut core = CoreCtx::new(nc);
    let r = {
        let mut c = C(&mut core);
        let to = NativeCtx_TO::from_ptr(RMut::new(&mut c), TD_Opaque);
        (f.func)(to, data)
    };
    core.finish(r.into_result().map_err(|m| m.into_string()))
}

#[sabi_extern_fn]
fn new_engine(ffi: bool) -> EngineBox {
    Engine_TO::from_value(Eng(orbit_api_core::Engine::new(ffi)), TD_Opaque)
}

#[export_root_module]
pub fn get_library() -> OrbitLib_Ref {
    OrbitLib { new_engine }.leak_into_prefix()
}

#[allow(dead_code)]
fn _types(_: RBox<()>) {}
