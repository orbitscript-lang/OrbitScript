//! The `shared` backend: the runtime in `liborbit`, reached through
//! `stabby`. The first call checks the exported function's type report
//! against the library's, and the library checks this build's
//! [`INTERFACE_HASH`]. No `unsafe` in this file.

use orbit_abi::{
    AError, AOptions, AValue, CtxMut, EngineBox, EngineDynMut, HostFn, INTERFACE_HASH,
    InterruptBox, InterruptDyn, LibraryBox, LibraryDyn, NativeCtxDyn, NativeCtxDynMut,
};
use stabby::{boxed::Box as SBox, result::Result as SResult, string::String as SString};

use crate::raw::{Kind, RawCtx, RawNative, RawVal};
use crate::{Error, Options};

#[stabby::import(name = "orbit_lib")]
unsafe extern "C" {
    pub fn orbit_abi_open(
        hash: u64,
    ) -> stabby::result::Result<orbit_abi::LibraryBox, stabby::string::String>;
}

/// The library, or why it refused this program.
fn library() -> Result<LibraryBox, String> {
    orbit_abi_open(INTERFACE_HASH).match_owned(Ok, |m| Err(m.to_string()))
}

fn to_a(v: &RawVal) -> AValue {
    match v {
        RawVal::Nil => AValue::Nil,
        RawVal::Bool(b) => AValue::Bool(*b),
        RawVal::Int(i) => AValue::Int(*i),
        RawVal::Float(f) => AValue::Float(*f),
        RawVal::Str(s) => AValue::Str(s.as_slice().into()),
        RawVal::Ref(id, Kind::Table) => AValue::Table(*id),
        RawVal::Ref(id, Kind::Function) => AValue::Function(*id),
        RawVal::Ref(id, Kind::Userdata) => AValue::Userdata(*id),
        RawVal::Ref(id, Kind::Thread) => AValue::Thread(*id),
    }
}

fn to_as(vs: &[RawVal]) -> Vec<AValue> {
    vs.iter().map(to_a).collect()
}

fn from_a(v: AValue) -> RawVal {
    match v {
        AValue::Nil => RawVal::Nil,
        AValue::Bool(b) => RawVal::Bool(b),
        AValue::Int(i) => RawVal::Int(i),
        AValue::Float(f) => RawVal::Float(f),
        AValue::Str(s) => RawVal::Str(s.to_vec()),
        AValue::Table(id) => RawVal::Ref(id, Kind::Table),
        AValue::Function(id) => RawVal::Ref(id, Kind::Function),
        AValue::Userdata(id) => RawVal::Ref(id, Kind::Userdata),
        AValue::Thread(id) => RawVal::Ref(id, Kind::Thread),
    }
}

fn from_as(vs: stabby::vec::Vec<AValue>) -> Vec<RawVal> {
    vs.into_iter().map(from_a).collect()
}

fn from_error(e: AError) -> Error {
    if e.syntax {
        Error::Syntax(e.message.to_string())
    } else {
        Error::Runtime {
            message: e.message.to_string(),
            traceback: e.has_traceback.then(|| e.traceback.to_string()),
        }
    }
}

/// An `AResult` as a `Result`, its value mapped by `f`. (A macro: stabby's
/// `Result` bounds don't survive a generic function.)
macro_rules! res {
    ($r:expr, $f:expr $(,)?) => {
        $r.match_owned(|v| Ok($f(v)), |e| Err(from_error(e)))
    };
}

pub struct Interrupt(InterruptBox);

impl Interrupt {
    pub fn request(&self) {
        self.0.request()
    }
    pub fn pending(&self) -> bool {
        self.0.pending()
    }
}

pub struct State(EngineBox);

impl State {
    pub fn new(options: &Options) -> Result<State, Error> {
        let lib = library().map_err(Error::Syntax)?;
        let flag = |on: bool, bit: u32| if on { bit } else { 0 };
        let opts = AOptions {
            flags: flag(options.ignore_env, orbit_abi::OPT_IGNORE_ENV)
                | flag(options.lock_cpath, orbit_abi::OPT_LOCK_CPATH)
                | flag(options.ffi, orbit_abi::OPT_FFI)
                | flag(options.gc_stress, orbit_abi::OPT_GC_STRESS)
                | flag(options.generational, orbit_abi::OPT_GENERATIONAL),
            memory_limit: options.memory_limit.map_or(u64::MAX, |b| b as u64),
        };
        res!(lib.new_state(opts), State)
    }
    pub fn release(&mut self, id: u32) {
        self.0.release(id)
    }
    pub fn load(&mut self, chunk: &[u8], source: &[u8]) -> Result<RawVal, Error> {
        res!(self.0.load(chunk.into(), source.into()), from_a)
    }
    pub fn load_file(&mut self, path: Option<&str>) -> Result<RawVal, Error> {
        res!(
            self.0.load_file(path.unwrap_or("").into(), path.is_none()),
            from_a,
        )
    }
    pub fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error> {
        let args = to_as(args);
        res!(self.0.call(&to_a(f), args.as_slice().into()), from_as)
    }
    pub fn call_with_traceback(
        &mut self,
        f: &RawVal,
        args: &[RawVal],
    ) -> Result<Vec<RawVal>, Error> {
        let args = to_as(args);
        res!(
            self.0.call_with_traceback(&to_a(f), args.as_slice().into()),
            from_as,
        )
    }
    pub fn global(&mut self, name: &str) -> RawVal {
        from_a(self.0.global(name.into()))
    }
    pub fn set_global(&mut self, name: &str, v: &RawVal) {
        self.0.set_global(name.into(), &to_a(v))
    }
    pub fn new_table(&mut self) -> RawVal {
        from_a(self.0.new_table())
    }
    pub fn get(&mut self, t: &RawVal, k: &RawVal) -> RawVal {
        from_a(self.0.get(&to_a(t), &to_a(k)))
    }
    pub fn set(&mut self, t: &RawVal, k: &RawVal, v: &RawVal) -> Result<(), Error> {
        res!(self.0.set(&to_a(t), &to_a(k), &to_a(v)), |_: bool| ())
    }
    pub fn len(&mut self, v: &RawVal) -> i64 {
        self.0.len(&to_a(v))
    }
    pub fn tostring(&mut self, v: &RawVal) -> Result<Vec<u8>, Error> {
        res!(self.0.tostring(&to_a(v)), |s: stabby::vec::Vec<u8>| s
            .to_vec())
    }
    pub fn function(&mut self, name: &str, f: RawNative) -> RawVal {
        from_a(self.0.function(name.into(), SBox::new(Native(f)).into()))
    }
    pub fn set_warnings(&mut self, on: bool) {
        self.0.set_warnings(on)
    }
    pub fn interrupt(&mut self) -> Interrupt {
        Interrupt(self.0.interrupt())
    }
    pub fn flush(&mut self) {
        self.0.flush()
    }
}

/// A program's native as the library holds it.
struct Native(RawNative);

impl HostFn for Native {
    extern "C" fn run<'a>(&self, ctx: CtxMut<'a>) -> SResult<usize, SString> {
        match (self.0)(&mut Ctx(ctx)) {
            Ok(n) => SResult::Ok(n),
            Err(m) => SResult::Err(m.as_str().into()),
        }
    }
}

/// [`RawCtx`] over the library's context.
struct Ctx<'a>(CtxMut<'a>);

impl RawCtx for Ctx<'_> {
    fn nargs(&self) -> usize {
        self.0.nargs()
    }
    fn arg(&mut self, i: usize) -> RawVal {
        from_a(self.0.arg(i))
    }
    fn push(&mut self, v: &RawVal) {
        self.0.push(&to_a(v))
    }
    fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error> {
        let args = to_as(args);
        res!(self.0.call(&to_a(f), args.as_slice().into()), from_as)
    }
}

pub mod tools {
    use orbit_abi::LibraryDyn;

    use super::library;
    use crate::tools::Compiled;

    pub fn read_chunk_file(path: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>), String> {
        let lib = library()?;
        lib.read_chunk_file(path.unwrap_or(b"").into(), path.is_none())
            .match_owned(
                |t| {
                    let (a, b): (stabby::vec::Vec<u8>, stabby::vec::Vec<u8>) = t.into();
                    Ok((a.to_vec(), b.to_vec()))
                },
                |m| Err(m.to_string()),
            )
    }

    pub fn chunk_id(source: &[u8]) -> String {
        match library() {
            Ok(lib) => lib.chunk_id(source.into()).to_string(),
            Err(_) => String::from_utf8_lossy(source).into_owned(),
        }
    }

    pub fn syntax_tree(bytes: &[u8], source: &[u8]) -> Result<Vec<u8>, String> {
        library()?
            .syntax_tree(bytes.into(), source.into())
            .match_owned(|v| Ok(v.to_vec()), |m| Err(m.to_string()))
    }

    pub fn hir(bytes: &[u8], source: &[u8]) -> Result<String, String> {
        library()?
            .hir(bytes.into(), source.into())
            .match_owned(|v| Ok(v.to_string()), |m| Err(m.to_string()))
    }

    pub fn compile(
        bytes: &[u8],
        source: &[u8],
        strip: bool,
        list: bool,
    ) -> Result<Compiled, String> {
        library()?
            .compile(bytes.into(), source.into(), strip, list)
            .match_owned(
                |c| {
                    Ok(Compiled {
                        chunk: c.chunk.to_vec(),
                        listing: c.listed.then(|| c.listing.to_string()),
                    })
                },
                |m| Err(m.to_string()),
            )
    }
}
