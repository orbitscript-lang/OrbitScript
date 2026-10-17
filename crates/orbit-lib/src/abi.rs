//! The Rust API's library half (ADR-10): `orbit-api`'s static runtime
//! behind `orbit-abi`'s stabby traits, exported as one function. No
//! `unsafe` here: stabby's macros generate the boundary.

use orbit_abi::{
    ACompiled, AError, AOptions, AResult, AValue, CtxMut, Engine, EngineBox, HostFnBox, HostFnDyn,
    INTERFACE_HASH, Interrupt, InterruptBox, Library, LibraryBox, NativeCtx,
};
use orbit_api::raw::{Kind, RawCtx, RawVal};
use orbit_api::{Error, Options};
use stabby::{
    boxed::Box as SBox, result::Result as SResult, slice::Slice, str::Str,
    string::String as SString, tuple::Tuple2, vec::Vec as SVec,
};

fn to_a(v: RawVal) -> AValue {
    match v {
        RawVal::Nil => AValue::Nil,
        RawVal::Bool(b) => AValue::Bool(b),
        RawVal::Int(i) => AValue::Int(i),
        RawVal::Float(f) => AValue::Float(f),
        RawVal::Str(s) => AValue::Str(s.as_slice().into()),
        RawVal::Ref(id, Kind::Table) => AValue::Table(id),
        RawVal::Ref(id, Kind::Function) => AValue::Function(id),
        RawVal::Ref(id, Kind::Userdata) => AValue::Userdata(id),
        RawVal::Ref(id, Kind::Thread) => AValue::Thread(id),
    }
}

fn to_as(vs: Vec<RawVal>) -> SVec<AValue> {
    vs.into_iter().map(to_a).collect()
}

fn from_a(v: &AValue) -> RawVal {
    match v {
        AValue::Nil => RawVal::Nil,
        AValue::Bool(b) => RawVal::Bool(*b),
        AValue::Int(i) => RawVal::Int(*i),
        AValue::Float(f) => RawVal::Float(*f),
        AValue::Str(s) => RawVal::Str(s.to_vec()),
        AValue::Table(id) => RawVal::Ref(*id, Kind::Table),
        AValue::Function(id) => RawVal::Ref(*id, Kind::Function),
        AValue::Userdata(id) => RawVal::Ref(*id, Kind::Userdata),
        AValue::Thread(id) => RawVal::Ref(*id, Kind::Thread),
    }
}

fn from_as(vs: &[AValue]) -> Vec<RawVal> {
    vs.iter().map(from_a).collect()
}

fn to_error(e: Error) -> AError {
    match e {
        Error::Syntax(m) => AError {
            syntax: true,
            message: m.as_str().into(),
            has_traceback: false,
            traceback: SString::default(),
        },
        Error::Runtime { message, traceback } => AError {
            syntax: false,
            message: message.as_str().into(),
            has_traceback: traceback.is_some(),
            traceback: traceback.as_deref().unwrap_or("").into(),
        },
    }
}

/// A `Result<T, Error>` as an [`AResult`], its value mapped by `f`. (A
/// macro: stabby's `Result` bounds don't survive a generic function.)
macro_rules! res {
    ($r:expr, $f:expr $(,)?) => {
        match $r {
            Ok(v) => SResult::Ok($f(v)),
            Err(e) => SResult::Err(to_error(e)),
        }
    };
}

/// A `Result<T, String>` at the boundary, its value mapped by `f`.
macro_rules! text {
    ($r:expr, $f:expr $(,)?) => {
        match $r {
            Ok(v) => SResult::Ok($f(v)),
            Err(m) => SResult::Err(SString::from(m.as_str())),
        }
    };
}

struct Eng(orbit_api::engine::Engine);

impl Engine for Eng {
    extern "C" fn release(&mut self, id: u32) {
        self.0.release(id)
    }
    extern "C" fn load<'a>(
        &mut self,
        chunk: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> AResult<AValue> {
        res!(self.0.load(&chunk, &source), to_a)
    }
    extern "C" fn load_file<'a>(&mut self, path: Str<'a>, stdin: bool) -> AResult<AValue> {
        let path: &str = &path;
        res!(self.0.load_file((!stdin).then_some(path)), to_a)
    }
    extern "C" fn call<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<SVec<AValue>> {
        res!(self.0.call(&from_a(f), &from_as(&args)), to_as)
    }
    extern "C" fn call_with_traceback<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<SVec<AValue>> {
        res!(
            self.0.call_with_traceback(&from_a(f), &from_as(&args)),
            to_as,
        )
    }
    extern "C" fn global<'a>(&mut self, name: Str<'a>) -> AValue {
        to_a(self.0.global(&name))
    }
    extern "C" fn set_global<'a>(&mut self, name: Str<'a>, v: &'a AValue) {
        self.0.set_global(&name, &from_a(v))
    }
    extern "C" fn new_table(&mut self) -> AValue {
        to_a(self.0.new_table())
    }
    extern "C" fn get<'a>(&mut self, t: &'a AValue, k: &'a AValue) -> AValue {
        to_a(self.0.get(&from_a(t), &from_a(k)))
    }
    extern "C" fn set<'a>(&mut self, t: &'a AValue, k: &'a AValue, v: &'a AValue) -> AResult<bool> {
        res!(self.0.set(&from_a(t), &from_a(k), &from_a(v)), |()| true)
    }
    extern "C" fn len(&mut self, v: &AValue) -> i64 {
        self.0.len(&from_a(v))
    }
    extern "C" fn tostring(&mut self, v: &AValue) -> AResult<SVec<u8>> {
        res!(self.0.tostring(&from_a(v)), |s: Vec<u8>| s
            .as_slice()
            .into())
    }
    extern "C" fn function<'a>(&mut self, name: Str<'a>, f: HostFnBox) -> AValue {
        let f = std::rc::Rc::new(f);
        let raw: orbit_api::raw::RawNative =
            Box::new(move |ctx: &mut dyn RawCtx| run_host(&f, ctx));
        to_a(self.0.function(&name, raw))
    }
    extern "C" fn set_warnings(&mut self, on: bool) {
        self.0.set_warnings(on)
    }
    extern "C" fn interrupt(&mut self) -> InterruptBox {
        SBox::new(Int(self.0.interrupt())).into()
    }
    extern "C" fn flush(&mut self) {
        self.0.flush()
    }
}

/// Run a program's native with a context it can call through.
fn run_host(f: &HostFnBox, ctx: &mut dyn RawCtx) -> Result<usize, String> {
    let mut c = Ctx(ctx);
    let ctx: CtxMut<'_> = (&mut c).into();
    f.run(ctx).match_owned(Ok, |m| Err(m.to_string()))
}

struct Int(orbit::Interrupt);

impl Interrupt for Int {
    extern "C" fn request(&self) {
        self.0.request()
    }
    extern "C" fn pending(&self) -> bool {
        self.0.pending()
    }
}

/// The library's context, as a program's native sees it.
struct Ctx<'a, 'b>(&'a mut (dyn RawCtx + 'b));

impl NativeCtx for Ctx<'_, '_> {
    extern "C" fn nargs(&self) -> usize {
        self.0.nargs()
    }
    extern "C" fn arg(&mut self, i: usize) -> AValue {
        to_a(self.0.arg(i))
    }
    extern "C" fn push(&mut self, v: &AValue) {
        self.0.push(&from_a(v))
    }
    extern "C" fn call<'a>(
        &mut self,
        f: &'a AValue,
        args: Slice<'a, AValue>,
    ) -> AResult<SVec<AValue>> {
        res!(self.0.call(&from_a(f), &from_as(&args)), to_as)
    }
}

struct Lib;

impl Library for Lib {
    extern "C" fn new_state(&self, o: AOptions) -> AResult<EngineBox> {
        let on = |bit: u32| o.flags & bit != 0;
        let options = Options {
            ignore_env: on(orbit_abi::OPT_IGNORE_ENV),
            lock_cpath: on(orbit_abi::OPT_LOCK_CPATH),
            ffi: on(orbit_abi::OPT_FFI),
            gc_stress: on(orbit_abi::OPT_GC_STRESS),
            generational: on(orbit_abi::OPT_GENERATIONAL),
            memory_limit: (o.memory_limit != u64::MAX).then_some(o.memory_limit as usize),
        };
        res!(orbit_api::engine::Engine::new(&options), |e| {
            SBox::new(Eng(e)).into()
        })
    }
    extern "C" fn read_chunk_file<'a>(
        &self,
        path: Slice<'a, u8>,
        stdin: bool,
    ) -> SResult<Tuple2<SVec<u8>, SVec<u8>>, SString> {
        let path: &[u8] = &path;
        text!(
            orbit_api::engine::tools::read_chunk_file((!stdin).then_some(path)),
            |(a, b): (Vec<u8>, Vec<u8>)| (a.as_slice().into(), b.as_slice().into()).into(),
        )
    }
    extern "C" fn chunk_id<'a>(&self, source: Slice<'a, u8>) -> SString {
        orbit_api::engine::tools::chunk_id(&source).as_str().into()
    }
    extern "C" fn syntax_tree<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> SResult<SVec<u8>, SString> {
        text!(
            orbit_api::engine::tools::syntax_tree(&bytes, &source),
            |v: Vec<u8>| v.as_slice().into(),
        )
    }
    extern "C" fn hir<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
    ) -> SResult<SString, SString> {
        text!(
            orbit_api::engine::tools::hir(&bytes, &source),
            |s: String| { s.as_str().into() }
        )
    }
    extern "C" fn compile<'a>(
        &self,
        bytes: Slice<'a, u8>,
        source: Slice<'a, u8>,
        strip: bool,
        list: bool,
    ) -> SResult<ACompiled, SString> {
        text!(
            orbit_api::engine::tools::compile(&bytes, &source, strip, list),
            |c: orbit_api::tools::Compiled| ACompiled {
                chunk: c.chunk.as_slice().into(),
                listed: c.listing.is_some(),
                listing: c.listing.as_deref().unwrap_or("").into(),
            },
        )
    }
}

/// The library's one Rust entry point: the [`Library`], unless the program
/// was built from a different version of `orbit-abi`.
#[stabby::export]
pub extern "C" fn orbit_abi_open(hash: u64) -> SResult<LibraryBox, SString> {
    if hash != INTERFACE_HASH {
        let m = format!(
            "this program was built for a different liborbit (interface {hash:016x}, library {INTERFACE_HASH:016x})"
        );
        return SResult::Err(m.as_str().into());
    }
    SResult::Ok(SBox::new(Lib).into())
}
