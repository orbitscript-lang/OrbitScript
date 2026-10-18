//! [`Scope`]: the stretch of host code in which scoped handles live.
//!
//! A scope is a window of the VM's stack, the same window a native function
//! works in: every handle a scope gives out is a value in that window, so
//! the collector sees it without any bookkeeping, and the window is cut back
//! when the scope ends. Handles carry the scope's lifetime, so the borrow
//! checker keeps them from outliving it.

use std::cell::{Ref, RefCell, RefMut};
use std::fmt;
use std::rc::Rc;

use orbit_runtime::object::{self as raw, NoMemory};
use orbit_runtime::vm::{LuaError, Meta, NativeCtx};

use crate::chunk::Chunk;
use crate::error::{Error, ErrorKind};
use crate::host::{self, HostFunction};
use crate::pin::{PinKind, Pinned};

use crate::value::{Function, IntoValue, Str, Table, Userdata, Value};

/// Per-isolate state that handles reach without the VM: pinned handles
/// queue their slots here when dropped, and compare it to tell isolates
/// apart.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    pub(crate) released: RefCell<Vec<u32>>,
}

/// Where scoped handles live: the body of a host function ([`Call`]
/// derefs to one), or [`Isolate::scope`] for host code outside any call.
///
/// Every handle the scope gives out keeps its value alive until the scope
/// ends. A loop that makes many handles grows the scope, so long-running
/// host loops should open a new scope per round.
///
/// [`Call`]: crate::Call
/// [`Isolate::scope`]: crate::Isolate::scope
pub struct Scope<'s> {
    ctx: RefCell<NativeCtx<'s>>,
    pub(crate) shared: Rc<Shared>,
}

impl fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

impl<'s> Scope<'s> {
    pub(crate) fn new(ctx: NativeCtx<'s>) -> Scope<'s> {
        let shared = ctx
            .vm
            .rt
            .ext
            .get::<Rc<Shared>>()
            .cloned()
            .expect("every VM the API reaches belongs to an isolate");
        Scope {
            ctx: RefCell::new(ctx),
            shared,
        }
    }

    /// The window, borrowed for one operation. Handles never hold the
    /// borrow, so two at once only happens if a borrow escapes (a held
    /// [`Str::as_bytes`]); that panics rather than aliasing.
    pub(crate) fn ctx(&self) -> RefMut<'_, NativeCtx<'s>> {
        self.ctx.borrow_mut()
    }

    pub(crate) fn try_ctx(&self) -> Option<RefMut<'_, NativeCtx<'s>>> {
        self.ctx.try_borrow_mut().ok()
    }

    pub(crate) fn ctx_ref(&self) -> Ref<'_, NativeCtx<'s>> {
        self.ctx.borrow()
    }

    /// A handle to `v`, which must already be in the window.
    pub(crate) fn wrap(&'s self, v: raw::Value) -> Value<'s> {
        Value::from_raw(self, v)
    }

    /// Put `v` in the window and give out a handle to it.
    pub(crate) fn root(&'s self, v: raw::Value) -> Value<'s> {
        self.ctx().push(v);
        self.wrap(v)
    }

    /// The window's last `n` values, as handles.
    pub(crate) fn last(&'s self, n: usize) -> Vec<Value<'s>> {
        let ctx = self.ctx();
        let len = ctx.len();
        let vals: Vec<raw::Value> = (len - n..len).map(|i| ctx.arg(i)).collect();
        drop(ctx);
        vals.into_iter().map(|v| self.wrap(v)).collect()
    }

    /// Keep `v` alive in a pin slot of its own.
    pub(crate) fn pin_raw<K: PinKind>(&self, v: raw::Value) -> Pinned<K> {
        let mut ctx = self.ctx();
        self.release_pins(&mut ctx);
        let slot = ctx.vm.pin_slot(v);
        Pinned::new(&self.shared, slot)
    }

    /// Free the slots of pinned handles dropped since the last call.
    pub(crate) fn release_pins(&self, ctx: &mut NativeCtx<'_>) {
        for slot in self.shared.released.borrow_mut().drain(..) {
            ctx.vm.free_pin_slot(slot);
        }
    }

    /// An error raised in the VM, as the API reports it, keeping its value.
    pub(crate) fn lua_error(&self, ctx: &mut NativeCtx<'_>, e: LuaError) -> Error {
        let message = ctx.vm.error_message(&e);
        let kind = if message == NoMemory::MESSAGE {
            ErrorKind::Memory
        } else if message.ends_with("interrupted!") {
            ErrorKind::Interrupted
        } else {
            ErrorKind::Runtime
        };
        self.release_pins(ctx);
        let slot = ctx.vm.pin_slot(e.value);
        Error::new(kind, message)
            .with_traceback(e.traceback)
            .with_value(Pinned::new(&self.shared, slot))
    }

    /// An API error, as the VM raises it: its value when it came from a
    /// script, otherwise its message.
    pub(crate) fn to_lua_error(&self, ctx: &mut NativeCtx<'_>, e: Error) -> LuaError {
        if let Some(p) = e.value()
            && let Some(slot) = p.slot_in(&self.shared)
        {
            return LuaError::new(ctx.vm.pin_slot_value(slot));
        }
        if e.kind() == ErrorKind::Memory {
            return ctx.no_memory();
        }
        LuaError::new(ctx.new_str(e.message().as_bytes()))
    }

    /// Finish an operation that may call a metamethod: run the call now and
    /// take its first result.
    pub(crate) fn finish_meta(
        &self,
        ctx: &mut NativeCtx<'_>,
        m: Result<Meta<raw::Value>, LuaError>,
    ) -> Result<raw::Value, Error> {
        match m {
            Ok(Meta::Done(v)) => Ok(v),
            Ok(Meta::Call { f, nargs }) => {
                let at = ctx.len() - nargs;
                ctx.insert(at, f);
                match ctx.call(nargs, None) {
                    Ok(n) => {
                        let v = if n > 0 { ctx.arg(at) } else { raw::Value::Nil };
                        ctx.truncate(at);
                        Ok(v)
                    }
                    Err(e) => Err(self.lua_error(ctx, e)),
                }
            }
            Err(e) => Err(self.lua_error(ctx, e)),
        }
    }

    // ---- public surface ---------------------------------------------------

    /// The registry: a table only host code reaches, where C modules keep
    /// their own state (Lua's `LUA_REGISTRYINDEX`).
    pub fn registry(&'s self) -> Table<'s> {
        let r = self.ctx().vm.rt.registry;
        Table::from_raw(self, self.root(raw::Value::Table(r)).raw())
    }

    /// Write to the isolate's standard output, the stream `print` and
    /// `io.stdout` share (C's `stdout` when the C API created the state).
    /// `f` must not use the scope: the window is borrowed while it runs.
    pub fn with_stdout<R>(&self, f: impl FnOnce(&mut dyn std::io::Write) -> R) -> R {
        let mut ctx = self.ctx();
        f(&mut *ctx.vm.rt.out)
    }

    /// `text` as a Lua numeral (decimal or hexadecimal, integer or float,
    /// spaces around allowed), as `tonumber` reads a string; `None` if it
    /// isn't one.
    pub fn parse_number(&'s self, text: &[u8]) -> Option<Value<'s>> {
        raw::number::str_to_number(text).map(|v| self.wrap(v))
    }

    /// The global table.
    pub fn globals(&'s self) -> Table<'s> {
        let g = self.ctx().vm.rt.globals;
        Table::from_raw(self, self.root(raw::Value::Table(g)).raw())
    }

    /// A new empty table.
    pub fn create_table(&'s self) -> Table<'s> {
        let mut ctx = self.ctx();
        let t = ctx.new_table(raw::Table::new());
        drop(ctx);
        Table::from_raw(self, self.root(raw::Value::Table(t)).raw())
    }

    /// A string with these bytes (any bytes, not only UTF-8).
    pub fn create_string(&'s self, bytes: impl AsRef<[u8]>) -> Str<'s> {
        let v = self.ctx().new_str(bytes.as_ref());
        Str::from_raw(self, self.root(v).raw())
    }

    /// A function scripts can call that runs `f` ([`HostFunction`]).
    pub fn create_function(&'s self, f: impl HostFunction) -> Function<'s> {
        let v = host::function_value(&mut self.ctx(), Rc::new(f));
        Function::from_raw(self, self.root(v).raw())
    }

    /// A userdata holding `value`, with no class: scripts can pass it
    /// around but not call methods on it. See [`Class`](crate::Class) for
    /// userdata with methods.
    pub fn create_userdata<T: 'static>(&'s self, value: T) -> Userdata<'s> {
        let u = host::new_userdata(&mut self.ctx(), value);
        Userdata::from_raw(self, self.root(raw::Value::UserData(u)).raw())
    }

    /// Convert a Rust value to a script value.
    pub fn value(&'s self, v: impl IntoValue<'s>) -> Result<Value<'s>, Error> {
        let r = v.into_raw(self)?;
        Ok(self.root(r))
    }

    /// Compile (or decode) `chunk` and give its main function, which runs
    /// the chunk when called.
    pub fn load(&'s self, chunk: Chunk<'_>) -> Result<Function<'s>, Error> {
        let f = crate::chunk::load(self, &chunk, None)?;
        Ok(Function::from_raw(self, self.root(f).raw()))
    }

    /// Link a chunk [`Compiler`](crate::Compiler) compiled, perhaps on
    /// another thread, and give its main function.
    pub fn load_compiled(&'s self, chunk: &crate::UnlinkedChunk) -> Function<'s> {
        let mut ctx = self.ctx();
        let env = raw::Value::Table(ctx.vm.rt.globals);
        let f = ctx.link(chunk, env);
        drop(ctx);
        Function::from_raw(self, self.root(f).raw())
    }

    /// [`Scope::load`], with `env` as the chunk's `_ENV` instead of the
    /// global table: usually a table, but any value will do, as for `load`.
    pub fn load_in(
        &'s self,
        chunk: Chunk<'_>,
        env: impl IntoValue<'s>,
    ) -> Result<Function<'s>, Error> {
        let env = self.value(env)?.raw();
        let f = crate::chunk::load(self, &chunk, Some(env))?;
        Ok(Function::from_raw(self, self.root(f).raw()))
    }
}
