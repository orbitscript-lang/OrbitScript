//! The runtime behind the `static` backend, in [`RawVal`]s: the program's
//! references are slots of one pinned table, so handing one out or
//! forgetting it costs a table store. `liborbit` wraps the same [`Engine`]
//! for the `shared` backend.

use std::rc::Rc;

use orbit::orbit_core::{Gc, Table, table};
use orbit::{CallOutcome, Lua, LuaError, MetaField, NativeCtx, NativeReturn, Value};

use crate::raw::{Kind, RawCtx, RawNative, RawVal};
use crate::{Error, Options};

/// The program's references, kept in the VM's extensions so natives reach
/// them too.
#[derive(Debug)]
struct Refs {
    /// Slot `i` holds reference `i`; pinned.
    table: Gc<Table>,
    free: Vec<u32>,
    next: u32,
}

/// Natives the program made, by their data word.
#[derive(Default)]
struct Natives(Vec<Rc<RawNative>>);

impl std::fmt::Debug for Natives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Natives({})", self.0.len())
    }
}

fn refs(vm: &mut orbit::Vm) -> &mut Refs {
    vm.rt
        .ext
        .get_or_insert_with::<Refs>(|| unreachable!("Engine::new makes the reference table"))
}

/// `v` as the program sees it: a reference takes a new slot.
fn to_raw(vm: &mut orbit::Vm, v: Value) -> RawVal {
    let kind = match v {
        Value::Nil => return RawVal::Nil,
        Value::Bool(b) => return RawVal::Bool(b),
        Value::Int(i) => return RawVal::Int(i),
        Value::Float(f) => return RawVal::Float(f),
        Value::Str(s) => return RawVal::Str(vm.str_bytes(s).to_vec()),
        Value::Table(_) => Kind::Table,
        Value::Closure(_) | Value::Native(_) => Kind::Function,
        Value::UserData(_) | Value::LightUserData(_) => Kind::Userdata,
        Value::Thread(_) => Kind::Thread,
    };
    let r = refs(vm);
    let id = match r.free.pop() {
        Some(id) => id,
        None => {
            r.next += 1;
            r.next
        }
    };
    let t = r.table;
    vm.table_set(t, Value::Int(id.into()), v);
    RawVal::Ref(id, kind)
}

fn to_raws(vm: &mut orbit::Vm, vs: Vec<Value>) -> Vec<RawVal> {
    vs.into_iter().map(|v| to_raw(vm, v)).collect()
}

/// `v` in the VM. A string made here is unrooted: see [`Pinned`].
fn from_raw(vm: &mut orbit::Vm, v: &RawVal) -> Value {
    match v {
        RawVal::Nil => Value::Nil,
        RawVal::Bool(b) => Value::Bool(*b),
        RawVal::Int(i) => Value::Int(*i),
        RawVal::Float(f) => Value::Float(*f),
        RawVal::Str(s) => vm.new_str(s),
        RawVal::Ref(id, _) => {
            let t = refs(vm).table;
            table::get(&vm.heap, t, Value::Int((*id).into()))
        }
    }
}

/// Values made from the program's, pinned until dropped by [`unpin`] so a
/// collection between making them and using them can't free the strings.
struct Pinned(Vec<Value>);

fn pinned(vm: &mut orbit::Vm, vs: &[&RawVal]) -> Pinned {
    let vals: Vec<Value> = vs
        .iter()
        .map(|v| {
            let v = from_raw(vm, v);
            if matches!(v, Value::Str(_)) {
                vm.pin(v);
            }
            v
        })
        .collect();
    Pinned(vals)
}

fn unpin(vm: &mut orbit::Vm, p: Pinned) {
    for &v in p.0.iter().rev() {
        if matches!(v, Value::Str(_)) {
            vm.unpin(v);
        }
    }
}

fn runtime(vm: &orbit::Vm, e: &LuaError) -> Error {
    Error::Runtime {
        message: vm.error_message(e),
        traceback: e.traceback.clone(),
    }
}

fn from_orbit(e: orbit::Error) -> Error {
    match e {
        orbit::Error::Syntax(m) => Error::Syntax(m),
        orbit::Error::Runtime { message, traceback } => Error::Runtime { message, traceback },
    }
}

/// A Lua state with the standard libraries open.
#[derive(Debug)]
pub struct Engine {
    lua: Lua,
    /// `docall` and `msghandler` (below), pinned; made on first use.
    traced: Option<(Value, Value)>,
}

impl Engine {
    pub fn new(options: &Options) -> Result<Engine, Error> {
        let mut lua = Lua::with_options(orbit::Options {
            ignore_env: options.ignore_env,
            lock_cpath: options.lock_cpath,
        });
        #[cfg(feature = "ffi")]
        if options.ffi {
            lua.enable_ffi();
        }
        let vm = lua.vm();
        if options.generational {
            vm.rt.gc.mode = orbit::GcMode::Generational;
        }
        if options.gc_stress {
            vm.heap.set_stress(true);
        }
        let t = vm.new_table();
        vm.pin(Value::Table(t));
        vm.rt.ext.get_or_insert_with(|| Refs {
            table: t,
            free: Vec::new(),
            next: 0,
        });
        lua.set_memory_limit(options.memory_limit);
        Ok(Engine { lua, traced: None })
    }

    fn vm(&mut self) -> &mut orbit::Vm {
        self.lua.vm()
    }

    pub fn release(&mut self, id: u32) {
        let vm = self.vm();
        let t = refs(vm).table;
        vm.table_set(t, Value::Int(id.into()), Value::Nil);
        refs(vm).free.push(id);
    }

    pub fn load(&mut self, chunk: &[u8], source: &[u8]) -> Result<RawVal, Error> {
        let f = self.lua.load_buffer(chunk, source).map_err(from_orbit)?;
        Ok(to_raw(self.vm(), f))
    }

    pub fn load_file(&mut self, path: Option<&str>) -> Result<RawVal, Error> {
        let (bytes, source) =
            orbit::read_chunk_file(path.map(str::as_bytes)).map_err(Error::Syntax)?;
        let source = if path.is_none() {
            b"=stdin".to_vec()
        } else {
            source
        };
        self.load(&bytes, &source)
    }

    pub fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error> {
        let vm = self.vm();
        let all: Vec<&RawVal> = std::iter::once(f).chain(args).collect();
        let p = pinned(vm, &all);
        let r = vm.call(p.0[0], &p.0[1..]);
        unpin(vm, p);
        match r {
            Ok(out) => Ok(to_raws(vm, out)),
            Err(e) => Err(runtime(vm, &e)),
        }
    }

    pub fn call_with_traceback(
        &mut self,
        f: &RawVal,
        args: &[RawVal],
    ) -> Result<Vec<RawVal>, Error> {
        let (xpcall, handler) = match self.traced {
            Some(t) => t,
            None => {
                let vm = self.vm();
                let x = vm.register_native("docall", docall);
                let h = vm.register_native("msghandler", msghandler);
                self.traced = Some((x, h));
                (x, h)
            }
        };
        let vm = self.vm();
        let all: Vec<&RawVal> = std::iter::once(f).chain(args).collect();
        let p = pinned(vm, &all);
        let mut call = Vec::with_capacity(p.0.len() + 1);
        call.extend([p.0[0], handler]);
        call.extend_from_slice(&p.0[1..]);
        let r = vm.call(xpcall, &call);
        unpin(vm, p);
        match r {
            Ok(mut r) if matches!(r.first(), Some(Value::Bool(true))) => {
                r.remove(0);
                Ok(to_raws(vm, r))
            }
            Ok(r) => {
                let e = r.get(1).copied().unwrap_or(Value::Nil);
                Err(Error::Runtime {
                    message: String::from_utf8_lossy(&vm.to_display(e)).into_owned(),
                    traceback: None,
                })
            }
            Err(e) => Err(Error::Runtime {
                message: error_text(vm, &e),
                traceback: None,
            }),
        }
    }

    pub fn global(&mut self, name: &str) -> RawVal {
        let vm = self.vm();
        let v = vm.global(name);
        to_raw(vm, v)
    }

    pub fn set_global(&mut self, name: &str, v: &RawVal) {
        let vm = self.vm();
        let p = pinned(vm, &[v]);
        vm.set_global(name, p.0[0]);
        unpin(vm, p);
    }

    pub fn new_table(&mut self) -> RawVal {
        let vm = self.vm();
        let t = vm.new_table();
        to_raw(vm, Value::Table(t))
    }

    pub fn get(&mut self, t: &RawVal, k: &RawVal) -> RawVal {
        let vm = self.vm();
        let p = pinned(vm, &[t, k]);
        let v = match p.0[0] {
            Value::Table(t) => table::get(&vm.heap, t, p.0[1]),
            _ => Value::Nil,
        };
        unpin(vm, p);
        to_raw(vm, v)
    }

    pub fn set(&mut self, t: &RawVal, k: &RawVal, v: &RawVal) -> Result<(), Error> {
        let vm = self.vm();
        let p = pinned(vm, &[t, k, v]);
        let r = match p.0[0] {
            Value::Table(t) => {
                table::set(&mut vm.heap, t, p.0[1], p.0[2]).map_err(|e| e.message().to_string())
            }
            other => Err(format!("attempt to index a {} value", other.type_name())),
        };
        unpin(vm, p);
        r.map_err(|message| Error::Runtime {
            message,
            traceback: None,
        })
    }

    pub fn len(&mut self, v: &RawVal) -> i64 {
        if let RawVal::Str(s) = v {
            return s.len() as i64;
        }
        let vm = self.vm();
        match from_raw(vm, v) {
            Value::Table(t) => vm.heap.get(t).len(&vm.heap),
            _ => 0,
        }
    }

    pub fn tostring(&mut self, v: &RawVal) -> Result<Vec<u8>, Error> {
        if let RawVal::Str(s) = v {
            return Ok(s.clone());
        }
        let vm = self.vm();
        let v = from_raw(vm, v);
        let mm = vm.rt.metamethod(&vm.heap, v, MetaField::ToString);
        if matches!(mm, Value::Nil) {
            return Ok(vm.to_display(v));
        }
        match vm.call(mm, &[v]) {
            Ok(r) => match r.first() {
                Some(&Value::Str(s)) => Ok(vm.str_bytes(s).to_vec()),
                _ => Err(Error::Runtime {
                    message: "'__tostring' must return a string".into(),
                    traceback: None,
                }),
            },
            Err(e) => Err(runtime(vm, &e)),
        }
    }

    pub fn function(&mut self, name: &str, f: RawNative) -> RawVal {
        let vm = self.vm();
        let list = &mut vm.rt.ext.get_or_insert_with(Natives::default).0;
        list.push(Rc::new(f));
        let i = list.len() as u64 - 1;
        // Natives live as long as the VM, and so does their name.
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        let v = vm.register_native_with_data(name, run_native, [i, 0]);
        to_raw(vm, v)
    }

    pub fn set_warnings(&mut self, on: bool) {
        self.vm().rt.warnings = on;
    }

    pub fn interrupt(&mut self) -> orbit::Interrupt {
        self.vm().interrupt_handle()
    }

    pub fn flush(&mut self) {
        self.lua.flush_files();
    }
}

impl Drop for Engine {
    /// Close the state as `lua_close` does: every object with a `__gc`
    /// still registered is finalized.
    fn drop(&mut self) {
        self.lua.flush_files();
        self.lua.vm().close();
        self.lua.flush_files();
    }
}

/// The text the standalone interpreter prints for an error that escaped
/// the message handler: `__tostring`'s, or the message and traceback.
fn error_text(vm: &mut orbit::Vm, e: &LuaError) -> String {
    if !matches!(e.value, Value::Str(_) | Value::Int(_) | Value::Float(_)) {
        let mm = vm.rt.metamethod(&vm.heap, e.value, MetaField::ToString);
        if !matches!(mm, Value::Nil) {
            vm.pin(e.value);
            let r = vm.call(mm, &[e.value]);
            vm.unpin(e.value);
            if let Ok(r) = r
                && let Some(&Value::Str(s)) = r.first()
            {
                return String::from_utf8_lossy(vm.str_bytes(s)).into_owned();
            }
        }
    }
    let mut text = vm.error_message(e);
    if let Some(tb) = &e.traceback {
        text.push('\n');
        text.push_str(tb);
    }
    text
}

/// A program's native: the closure registered under its data word.
fn run_native(nc: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let i = nc.native_data()[0] as usize;
    let f = nc.vm.rt.ext.get::<Natives>().map(|n| n.0[i].clone());
    let f = f.expect("registered by Engine::function");
    let r = f(&mut Ctx { nc: &mut *nc });
    match r {
        Ok(n) => Ok(NativeReturn::Return(n)),
        Err(m) => Err(nc.error(m)),
    }
}

/// [`RawCtx`] over a native's context.
struct Ctx<'a, 'b> {
    nc: &'a mut NativeCtx<'b>,
}

impl RawCtx for Ctx<'_, '_> {
    fn nargs(&self) -> usize {
        self.nc.nargs()
    }

    fn arg(&mut self, i: usize) -> RawVal {
        let v = self.nc.arg(i);
        to_raw(self.nc.vm, v)
    }

    fn push(&mut self, v: &RawVal) {
        // Pushed values are on the stack, so rooted.
        let v = from_raw(self.nc.vm, v);
        self.nc.push(v);
    }

    fn call(&mut self, f: &RawVal, args: &[RawVal]) -> Result<Vec<RawVal>, Error> {
        let mark = self.nc.len();
        for v in std::iter::once(f).chain(args) {
            let v = from_raw(self.nc.vm, v);
            self.nc.push(v);
        }
        match self.nc.call(args.len(), None) {
            Ok(n) => {
                let base = self.nc.len() - n;
                let out: Vec<Value> = self.nc.window()[base..].to_vec();
                let out = to_raws(self.nc.vm, out);
                self.nc.truncate(mark);
                Ok(out)
            }
            Err(e) => {
                self.nc.truncate(mark);
                Err(runtime(self.nc.vm, &e))
            }
        }
    }
}

/// `docall(f, handler, args...)`: `f(args...)` under `handler`, as
/// `xpcall` does, from a native the traceback names only as `[C]: in ?`.
fn docall(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let (f, h) = (ctx.arg(0), ctx.arg(1));
    ctx.set(0, h);
    ctx.set(1, Value::Bool(true));
    Ok(NativeReturn::CallHandled {
        f,
        nargs: ctx.nargs() - 2,
        k: docall_done,
    })
}

fn docall_done(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> Result<NativeReturn, LuaError> {
    match out {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n + 1)),
        CallOutcome::Err(e) => {
            ctx.truncate(0);
            ctx.push(Value::Bool(false));
            ctx.push(e);
            Ok(NativeReturn::Return(2))
        }
    }
}

/// `lua`'s message handler: an error object with `__tostring` becomes that
/// text; anything else becomes its message plus a traceback.
fn msghandler(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let e = ctx.arg(0);
    let text = match e {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let mut b = Vec::new();
            ctx.display(e, &mut b);
            String::from_utf8_lossy(&b).into_owned()
        }
        _ => {
            let mm = ctx.vm.rt.metamethod(&ctx.vm.heap, e, MetaField::ToString);
            if !matches!(mm, Value::Nil) {
                ctx.push(e);
                return Ok(NativeReturn::CallThen {
                    f: mm,
                    nargs: 1,
                    k: tostring_done,
                    protected: false,
                });
            }
            format!("(error object is a {} value)", e.type_name())
        }
    };
    let text = format!("{text}\n{}", ctx.traceback(1));
    let s = ctx.new_str(text.as_bytes());
    ctx.ret1(s)
}

fn tostring_done(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> Result<NativeReturn, LuaError> {
    let n = match out {
        CallOutcome::Ok(n) => n,
        CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    match (n > 0).then(|| ctx.arg(ctx.len() - n)) {
        Some(s @ Value::Str(_)) => ctx.ret1(s),
        _ => Err(ctx.error("'__tostring' must return a string")),
    }
}

/// The compiler's tools, for `orbitc`.
pub mod tools {
    use std::io::Write;

    use orbit_compiler::bytecode::{UnlinkedChunk, format, verify};

    pub use orbit::{chunk_id, read_chunk_file};

    use crate::tools::Compiled;

    fn is_binary(bytes: &[u8]) -> bool {
        bytes.first() == format::SIGNATURE.first()
    }

    /// Whether `source` names OrbitScript source (not a binary chunk).
    fn is_orb(bytes: &[u8], source: &[u8]) -> bool {
        orbit::is_orbitscript_file(source) && !is_binary(bytes)
    }

    pub fn syntax_tree(bytes: &[u8], source: &[u8]) -> Result<Vec<u8>, String> {
        let name = chunk_id(source);
        if is_orb(bytes, source) {
            return Err(format!(
                "{name}: --ast is not available for OrbitScript yet"
            ));
        }
        let options = orbit_lua_syntax::ParseOptions::default();
        let ast = orbit_lua_syntax::parse_with(&name, bytes, &options)
            .map_err(|d| d.to_lua_message(&name))?;
        let mut out = Vec::new();
        let _ = out.write_all(&orbit_lua_syntax::unparse(&ast));
        Ok(out)
    }

    pub fn hir(bytes: &[u8], source: &[u8]) -> Result<String, String> {
        let name = chunk_id(source);
        let hir = if is_orb(bytes, source) {
            let options = orbit_orb_syntax::ParseOptions::default();
            orbit_orb_syntax::to_hir(&name, bytes, &options).map_err(|d| d.to_message(&name))?
        } else {
            let options = orbit_lua_syntax::ParseOptions::default();
            orbit_lua_syntax::to_hir(&name, bytes, &options).map_err(|d| d.to_lua_message(&name))?
        };
        Ok(orbit_hir::print(&hir))
    }

    pub fn compile(
        bytes: &[u8],
        source: &[u8],
        strip: bool,
        list: bool,
    ) -> Result<Compiled, String> {
        let chunk = load(bytes, source)?;
        Ok(Compiled {
            listing: list.then(|| orbit_compiler::disassemble(&chunk)),
            chunk: format::encode(&chunk, strip),
        })
    }

    /// Compile source, or decode and check a binary chunk (for listing it,
    /// or writing it again stripped).
    fn load(bytes: &[u8], source: &[u8]) -> Result<UnlinkedChunk, String> {
        let name = chunk_id(source);
        if is_binary(bytes) {
            let bad = |e: &dyn std::fmt::Display| format!("{name}: bad binary format ({e})");
            let chunk = format::decode(bytes).map_err(|e| bad(&e))?;
            verify::verify(&chunk, verify::Level::STRONGEST).map_err(|e| bad(&e))?;
            return Ok(chunk);
        }
        let mut chunk = if is_orb(bytes, source) {
            let options = orbit_orb_syntax::ParseOptions::default();
            orbit::compile_orbitscript(bytes, &name, &options).map_err(|e| e.to_string())?
        } else {
            let options = orbit_lua_syntax::ParseOptions::default();
            orbit::compile(bytes, &name, &options).map_err(|e| e.to_string())?
        };
        // As `load` does: the source as given, so a chunk read back names it.
        chunk.source = source.into();
        Ok(chunk)
    }
}
