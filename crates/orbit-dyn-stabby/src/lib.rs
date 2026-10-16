//! SPIKE: liborbit's side of the stabby projection. No `unsafe` here:
//! stabby's macros generate the boundary.

use std::rc::Rc;

use orbit_api_core::orbit::{LuaError, NativeCtx as VmCtx, NativeReturn};
use orbit_api_core::{Ctx as CoreCtx, CtxApi, Val};
use orbit_stabby_iface::{CtxMut, Engine, EngineBox, HostFnBox, HostFnDyn, NativeCtx, SVal, Vals};
use stabby::{
    boxed::Box as SBox, result::Result as SResult, slice::Slice, str::Str,
    string::String as SString, vec::Vec as SVec,
};

fn to_s(v: Val) -> SVal {
    match v {
        Val::Nil => SVal::Nil,
        Val::Bool(b) => SVal::Bool(b),
        Val::Int(i) => SVal::Int(i),
        Val::Float(f) => SVal::Float(f),
        Val::Str(s) => SVal::Str(s.as_slice().into()),
        Val::Ref(r) => SVal::Ref(r),
    }
}

fn from_s(v: &SVal) -> Val {
    match v {
        SVal::Nil => Val::Nil,
        SVal::Bool(b) => Val::Bool(*b),
        SVal::Int(i) => Val::Int(*i),
        SVal::Float(f) => Val::Float(*f),
        SVal::Str(s) => Val::Str(s.to_vec()),
        SVal::Ref(r) => Val::Ref(*r),
    }
}

fn err(m: String) -> SString {
    m.as_str().into()
}

fn res(r: Result<Vec<Val>, String>) -> Vals {
    match r {
        Ok(v) => SResult::Ok(v.into_iter().map(to_s).collect()),
        Err(m) => SResult::Err(err(m)),
    }
}

struct Eng(orbit_api_core::Engine);

/// Natives registered through this library. `Rc` so a native can run while
/// a Lua function it calls runs it again.
#[derive(Default)]
struct Natives(Vec<Rc<HostFnBox>>);

impl Engine for Eng {
    extern "C" fn load<'a>(&mut self, src: Slice<'a, u8>, name: Str<'a>) -> SResult<SVal, SString> {
        match self.0.load(&src, &name) {
            Ok(v) => SResult::Ok(to_s(v)),
            Err(m) => SResult::Err(err(m)),
        }
    }
    extern "C" fn call<'a>(&mut self, f: SVal, args: Slice<'a, SVal>) -> Vals {
        let args: Vec<Val> = args.iter().map(from_s).collect();
        res(self.0.call(&from_s(&f), &args))
    }
    extern "C" fn set_global<'a>(&mut self, name: Str<'a>, v: SVal) {
        self.0.set_global(&name, &from_s(&v))
    }
    extern "C" fn global<'a>(&mut self, name: Str<'a>) -> SVal {
        to_s(self.0.global(&name))
    }
    extern "C" fn new_table(&mut self) -> SVal {
        to_s(self.0.new_table())
    }
    extern "C" fn table_set(&mut self, t: SVal, k: SVal, v: SVal) {
        self.0.table_set(&from_s(&t), &from_s(&k), &from_s(&v))
    }
    extern "C" fn release(&mut self, r: u32) {
        self.0.release(r)
    }
    extern "C" fn tostring(&mut self, v: SVal) -> SVec<u8> {
        self.0.tostring(&from_s(&v)).as_slice().into()
    }
    extern "C" fn register<'a>(&mut self, name: Str<'a>, f: HostFnBox) {
        let list = &mut self.0.ext::<Natives>().0;
        list.push(Rc::new(f));
        let i = list.len() as u64 - 1;
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        self.0.register_raw(name, run_native, [i, 0]);
    }
}

struct C<'a, 'b, 'c>(&'a mut CoreCtx<'b, 'c>);

impl NativeCtx for C<'_, '_, '_> {
    extern "C" fn nargs(&self) -> usize {
        self.0.nargs()
    }
    extern "C" fn arg(&mut self, i: usize) -> SVal {
        to_s(self.0.arg(i))
    }
    extern "C" fn push(&mut self, v: SVal) {
        self.0.push(&from_s(&v))
    }
    extern "C" fn call<'a>(&mut self, f: SVal, args: Slice<'a, SVal>) -> Vals {
        let args: Vec<Val> = args.iter().map(from_s).collect();
        res(self.0.call(&from_s(&f), &args))
    }
}

fn run_native(nc: &mut VmCtx<'_>) -> Result<NativeReturn, LuaError> {
    let i = nc.native_data()[0] as usize;
    let f = nc
        .vm
        .rt
        .ext
        .get::<Natives>()
        .map(|n| n.0[i].clone())
        .expect("registered");
    let mut core = CoreCtx::new(nc);
    let r = {
        let mut c = C(&mut core);
        let ctx: CtxMut<'_> = (&mut c).into();
        f.run(ctx)
    };
    let r = match r.match_owned(Ok, Err) {
        Ok(n) => Ok(n),
        Err(m) => Err(m.to_string()),
    };
    core.finish(r)
}

#[stabby::export]
pub extern "C" fn orbit_stabby_new_engine(ffi: bool) -> EngineBox {
    SBox::new(Eng(orbit_api_core::Engine::new(ffi))).into()
}
