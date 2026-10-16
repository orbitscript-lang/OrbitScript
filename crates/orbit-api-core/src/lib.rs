//! SPIKE: the embedding API the projections carry across a library
//! boundary. Values cross as [`Val`]: scalars and strings by copy, tables and
//! functions as numbered references the engine keeps alive until released.

use orbit::{LuaError, NativeCtx, NativeReturn, Value};

pub use orbit_api_types::{Ctx as CtxApi, NativeFn, Val};

/// References the program holds, rooted on the VM.
#[derive(Debug, Default)]
struct Refs {
    slots: Vec<Value>,
    free: Vec<u32>,
}

fn to_val(vm: &mut orbit::Vm, v: Value) -> Val {
    match v {
        Value::Nil => Val::Nil,
        Value::Bool(b) => Val::Bool(b),
        Value::Int(i) => Val::Int(i),
        Value::Float(f) => Val::Float(f),
        Value::Str(s) => Val::Str(vm.str_bytes(s).to_vec()),
        v => {
            vm.pin(v);
            let refs = vm.rt.ext.get_or_insert_with(Refs::default);
            match refs.free.pop() {
                Some(i) => {
                    refs.slots[i as usize] = v;
                    Val::Ref(i)
                }
                None => {
                    refs.slots.push(v);
                    Val::Ref(refs.slots.len() as u32 - 1)
                }
            }
        }
    }
}

fn from_val(vm: &mut orbit::Vm, v: &Val) -> Value {
    match v {
        Val::Nil => Value::Nil,
        Val::Bool(b) => Value::Bool(*b),
        Val::Int(i) => Value::Int(*i),
        Val::Float(f) => Value::Float(*f),
        Val::Str(s) => vm.new_str(s),
        Val::Ref(i) => vm
            .rt
            .ext
            .get_or_insert_with(Refs::default)
            .slots
            .get(*i as usize)
            .copied()
            .unwrap_or(Value::Nil),
    }
}

fn error_text(e: orbit::Error) -> String {
    match e {
        orbit::Error::Syntax(m) => m,
        orbit::Error::Runtime { message, traceback } => match traceback {
            Some(t) => format!("{message}\n{t}"),
            None => message,
        },
    }
}

/// A Lua state with every library open.
#[derive(Debug)]
pub struct Engine {
    lua: orbit::Lua,
}

impl Engine {
    pub fn new(ffi: bool) -> Engine {
        let mut lua = orbit::Lua::new();
        if ffi {
            lua.enable_ffi();
        }
        lua.vm().rt.gc.mode = orbit::GcMode::Generational;
        Engine { lua }
    }

    /// Compile a chunk (`name` as `loadfile` takes it: `@file`, `=name`).
    pub fn load(&mut self, src: &[u8], name: &str) -> Result<Val, String> {
        let f = self
            .lua
            .load_buffer(src, name.as_bytes())
            .map_err(error_text)?;
        Ok(to_val(self.lua.vm(), f))
    }

    pub fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let vm = self.lua.vm();
        let f = from_val(vm, f);
        let args: Vec<Value> = args.iter().map(|a| from_val(vm, a)).collect();
        // Strings made for the arguments are unrooted until the call has them.
        let out = self.lua.call(f, &args).map_err(error_text)?;
        let vm = self.lua.vm();
        Ok(out.into_iter().map(|v| to_val(vm, v)).collect())
    }

    pub fn set_global(&mut self, name: &str, v: &Val) {
        let vm = self.lua.vm();
        let v = from_val(vm, v);
        vm.set_global(name, v);
    }

    pub fn global(&mut self, name: &str) -> Val {
        let vm = self.lua.vm();
        let v = vm.global(name);
        to_val(vm, v)
    }

    pub fn new_table(&mut self) -> Val {
        let vm = self.lua.vm();
        let t = vm.new_table();
        to_val(vm, Value::Table(t))
    }

    /// `t[k] = v` (raw). Does nothing if `t` is not a table or `k` is nil.
    pub fn table_set(&mut self, t: &Val, k: &Val, v: &Val) {
        let vm = self.lua.vm();
        let (t, k, v) = (from_val(vm, t), from_val(vm, k), from_val(vm, v));
        if let (Value::Table(t), false) = (t, matches!(k, Value::Nil)) {
            vm.table_set(t, k, v);
        }
    }

    /// Let the engine forget reference `r`.
    pub fn release(&mut self, r: u32) {
        let vm = self.lua.vm();
        let refs = vm.rt.ext.get_or_insert_with(Refs::default);
        if let Some(v) = refs.slots.get(r as usize).copied() {
            refs.slots[r as usize] = Value::Nil;
            refs.free.push(r);
            vm.unpin(v);
        }
    }

    pub fn tostring(&mut self, v: &Val) -> Vec<u8> {
        let vm = self.lua.vm();
        let v = from_val(vm, v);
        vm.to_display(v)
    }

    /// Register a native as global `name`. `f` runs it with `data`, the
    /// native's two data words (a projection keeps its function pointer
    /// there).
    pub fn register_raw(&mut self, name: &'static str, f: orbit::NativeFnRaw, data: [u64; 2]) {
        let vm = self.lua.vm();
        let v = vm.register_native_with_data(name, f, data);
        vm.set_global(name, v);
    }

    /// Register a native written against this crate (the static
    /// projection).
    pub fn register(&mut self, name: &'static str, f: NativeFn) {
        let list = &mut self
            .lua
            .vm()
            .rt
            .ext
            .get_or_insert_with(StaticNatives::default)
            .0;
        list.push(f);
        let i = list.len() as u64 - 1;
        self.register_raw(name, run_static, [i, 0]);
    }

    /// Per-state data a projection keeps (its natives' targets).
    pub fn ext<T: Default + 'static>(&mut self) -> &mut T {
        self.lua.vm().rt.ext.get_or_insert_with(T::default)
    }

    pub fn flush(&mut self) {
        self.lua.flush_files();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.lua.flush_files();
        self.lua.vm().close();
    }
}

#[derive(Debug, Default)]
struct StaticNatives(Vec<NativeFn>);

fn run_static(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let i = ctx.native_data()[0] as usize;
    let f = ctx.vm.rt.ext.get::<StaticNatives>().map(|s| s.0[i]);
    let f = f.expect("registered with Engine::register");
    let mut c = Ctx::new(ctx);
    let r = f(&mut c);
    c.finish(r)
}

/// What a native sees: its arguments and a way to return values and call
/// back into Lua.
#[derive(Debug)]
pub struct Ctx<'a, 'b> {
    nc: &'a mut NativeCtx<'b>,
}

impl<'a, 'b> Ctx<'a, 'b> {
    pub fn new(nc: &'a mut NativeCtx<'b>) -> Self {
        Ctx { nc }
    }

    /// Finish a native: `Ok(n)` returns the last `n` pushed values.
    pub fn finish(&mut self, r: Result<usize, String>) -> Result<NativeReturn, LuaError> {
        match r {
            Ok(n) => Ok(NativeReturn::Return(n)),
            Err(m) => Err(self.nc.error(m)),
        }
    }
}

impl CtxApi for Ctx<'_, '_> {
    fn nargs(&self) -> usize {
        self.nc.nargs()
    }

    fn arg(&mut self, i: usize) -> Val {
        let v = self.nc.arg(i);
        to_val(self.nc.vm, v)
    }

    fn push(&mut self, v: &Val) {
        let v = from_val(self.nc.vm, v);
        self.nc.push(v);
    }

    fn call(&mut self, f: &Val, args: &[Val]) -> Result<Vec<Val>, String> {
        let mark = self.nc.len();
        let fv = from_val(self.nc.vm, f);
        self.nc.push(fv);
        for a in args {
            let a = from_val(self.nc.vm, a);
            self.nc.push(a);
        }
        match self.nc.call(args.len(), None) {
            Ok(n) => {
                let base = self.nc.len() - n;
                let out: Vec<Value> = self.nc.window()[base..].to_vec();
                self.nc.truncate(mark);
                Ok(out.into_iter().map(|v| to_val(self.nc.vm, v)).collect())
            }
            Err(e) => {
                self.nc.truncate(mark);
                Err(self.nc.vm.error_message(&e))
            }
        }
    }
}
pub use orbit;
