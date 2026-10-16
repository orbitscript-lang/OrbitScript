//! SPIKE: the boundary between liborbit and programs, described with
//! `abi_stable`: a root module with one constructor, and two `#[sabi_trait]`
//! traits checked for layout when the library is loaded.

use abi_stable::{
    StableAbi, declare_root_module_statics,
    library::RootModule,
    package_version_strings, sabi_trait,
    sabi_types::{RMut, VersionStrings},
    std_types::{RBox, RResult, RSlice, RStr, RString, RVec},
};

/// A value at the boundary (`orbit_api_types::Val`).
#[repr(C, u8)]
#[derive(StableAbi, Debug, Clone)]
pub enum FVal {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(RVec<u8>),
    Ref(u32),
}

/// What a native sees.
#[sabi_trait]
pub trait NativeCtx {
    fn nargs(&self) -> usize;
    fn arg(&mut self, i: usize) -> FVal;
    fn push(&mut self, v: FVal);
    #[sabi(last_prefix_field)]
    fn call(&mut self, f: FVal, args: RSlice<'_, FVal>) -> RResult<RVec<FVal>, RString>;
}

pub type CtxRef<'a> = NativeCtx_TO<'a, RMut<'a, ()>>;

/// A program's native as the library calls it; `data` is the program's own.
/// (abi_stable wants function pointers in parameters wrapped like this.)
#[repr(transparent)]
#[derive(StableAbi, Clone, Copy)]
pub struct NativeExt {
    pub func:
        extern "C" fn(ctx: NativeCtx_TO<'_, RMut<'_, ()>>, data: usize) -> RResult<usize, RString>,
}

/// A Lua state.
#[sabi_trait]
pub trait Engine {
    fn load(&mut self, src: RSlice<'_, u8>, name: RStr<'_>) -> RResult<FVal, RString>;
    fn call(&mut self, f: FVal, args: RSlice<'_, FVal>) -> RResult<RVec<FVal>, RString>;
    fn set_global(&mut self, name: RStr<'_>, v: FVal);
    fn global(&mut self, name: RStr<'_>) -> FVal;
    fn new_table(&mut self) -> FVal;
    fn table_set(&mut self, t: FVal, k: FVal, v: FVal);
    fn release(&mut self, r: u32);
    fn tostring(&mut self, v: FVal) -> RVec<u8>;
    #[sabi(last_prefix_field)]
    fn register(&mut self, name: RStr<'_>, f: NativeExt, data: usize);
}

pub type EngineBox = Engine_TO<'static, RBox<()>>;

/// The library's root module.
#[repr(C)]
#[derive(StableAbi)]
#[sabi(kind(Prefix(prefix_ref = OrbitLib_Ref)))]
#[sabi(missing_field(panic))]
pub struct OrbitLib {
    #[sabi(last_prefix_field)]
    pub new_engine: extern "C" fn(ffi: bool) -> EngineBox,
}

impl RootModule for OrbitLib_Ref {
    declare_root_module_statics! {OrbitLib_Ref}
    const BASE_NAME: &'static str = "orbit_abi";
    const NAME: &'static str = "orbit_abi";
    const VERSION_STRINGS: VersionStrings = package_version_strings!();
}
