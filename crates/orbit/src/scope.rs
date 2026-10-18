//! [`Scope`]: the stretch of host code in which scoped handles live.
//!
//! A scope is a window of the VM's stack, the same window a native function
//! works in: every handle a scope gives out is a slot in that window, so
//! the collector sees its value without any bookkeeping, and the window is
//! cut back when the scope ends. Handles carry the scope's lifetime, so the
//! borrow checker keeps them from outliving it.
//!
//! The typed API is written over the stack API ([`Stack`]): a scope holds
//! the window's stack and every handle is a slot index in it.

use std::cell::{Ref, RefCell, RefMut};
use std::fmt;
use std::rc::Rc;

use crate::chunk::Chunk;
use crate::error::{Error, ErrorKind};
use crate::host::HostFunction;
use crate::pin::{PinKind, Pinned};
use crate::stack::{REGISTRY_INDEX, RIDX_GLOBALS, Raised, Stack, Status, Type};
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
    stack: RefCell<Stack<'s>>,
    pub(crate) shared: Rc<Shared>,
    /// Where the program keeps the isolate's host values.
    #[cfg(shared_backend)]
    pub(crate) host: Rc<crate::stack::backend::HostState>,
}

impl fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").finish_non_exhaustive()
    }
}

impl<'s> Scope<'s> {
    pub(crate) fn new(stack: Stack<'s>) -> Scope<'s> {
        let shared = stack
            .find_extension::<Rc<Shared>, _>(Rc::clone)
            .expect("every VM the API reaches belongs to an isolate");
        let scope = Scope {
            #[cfg(shared_backend)]
            host: Rc::clone(&stack.host),
            stack: RefCell::new(stack),
            shared,
        };
        scope.release_pins(&mut scope.stack());
        scope
    }

    /// The window, borrowed for one operation. Handles never hold the
    /// borrow, so two at once only happens if a borrow escapes (a held
    /// [`Str::as_bytes`]); that panics rather than aliasing.
    pub(crate) fn stack(&self) -> RefMut<'_, Stack<'s>> {
        self.stack.borrow_mut()
    }

    pub(crate) fn try_stack(&self) -> Option<RefMut<'_, Stack<'s>>> {
        self.stack.try_borrow_mut().ok()
    }

    pub(crate) fn stack_ref(&self) -> Ref<'_, Stack<'s>> {
        self.stack.borrow()
    }

    /// The handle to the value in slot `idx`.
    pub(crate) fn at(&'s self, idx: i32) -> Value<'s> {
        Value::at(self, idx)
    }

    /// A handle to the value on top of the window.
    pub(crate) fn top(&'s self) -> Value<'s> {
        let top = self.stack_ref().top();
        self.at(top)
    }

    /// The window's last `n` values, as handles.
    pub(crate) fn last(&'s self, n: usize) -> Vec<Value<'s>> {
        let top = self.stack_ref().top();
        (top + 1 - n as i32..=top).map(|i| self.at(i)).collect()
    }

    /// Keep the value in slot `idx` alive in a pin slot of its own.
    pub(crate) fn pin_slot<K: PinKind>(&self, idx: i32) -> Pinned<K> {
        let mut stack = self.stack();
        self.release_pins(&mut stack);
        let slot = stack.pin(idx);
        Pinned::new(&self.shared, slot)
    }

    /// Free the slots of pinned handles dropped since the last call.
    pub(crate) fn release_pins(&self, stack: &mut Stack<'_>) {
        for slot in self.shared.released.borrow_mut().drain(..) {
            stack.unpin(slot);
        }
    }

    /// An error raised in the VM, as the API reports it, keeping its value.
    pub(crate) fn error_of(&self, stack: &mut Stack<'_>, e: Raised) -> Error {
        let kind = match stack.status_of(&e) {
            Status::Memory => ErrorKind::Memory,
            _ => ErrorKind::Runtime,
        };
        let (message, traceback) = stack.take_raised(e);
        let kind = if kind == ErrorKind::Runtime && message.ends_with("interrupted!") {
            ErrorKind::Interrupted
        } else {
            kind
        };
        self.release_pins(stack);
        let slot = stack.pin(-1);
        let _ = stack.set_top(-2);
        Error::new(kind, message)
            .with_traceback(traceback)
            .with_value(Pinned::new(&self.shared, slot))
    }

    /// An API error, as the VM raises it: its value when it came from a
    /// script, otherwise its message.
    pub(crate) fn raise(&self, stack: &mut Stack<'_>, e: Error) -> Raised {
        if let Some(p) = e.value()
            && let Some(slot) = p.slot_in(&self.shared)
        {
            stack.push_pinned(slot);
            return stack.error_from_top();
        }
        if e.kind() == ErrorKind::Memory {
            return stack.memory_error();
        }
        stack.push_bytes(e.message().as_bytes());
        stack.error_from_top()
    }

    /// Run `op` on the window, turning a raised error into an [`Error`].
    pub(crate) fn run<R>(
        &self,
        op: impl FnOnce(&mut Stack<'s>) -> Result<R, Raised>,
    ) -> Result<R, Error> {
        let mut stack = self.stack();
        op(&mut stack).map_err(|e| self.error_of(&mut stack, e))
    }

    // ---- public surface ---------------------------------------------------

    /// The registry: a table only host code reaches, where C modules keep
    /// their own state (Lua's `LUA_REGISTRYINDEX`).
    pub fn registry(&'s self) -> Table<'s> {
        self.stack().push_copy(REGISTRY_INDEX);
        Table::top(self)
    }

    /// Write to the isolate's standard output, the stream `print` and
    /// `io.stdout` share (C's `stdout` when the C API created the state).
    /// `f` must not use the scope: the window is borrowed while it runs.
    pub fn with_stdout<R>(&self, f: impl FnOnce(&mut dyn std::io::Write) -> R) -> R {
        self.stack().with_stdout(f)
    }

    /// `text` as a Lua numeral (decimal or hexadecimal, integer or float,
    /// spaces around allowed), as `tonumber` reads a string; `None` if it
    /// isn't one.
    pub fn parse_number(&'s self, text: &[u8]) -> Option<Value<'s>> {
        let mut stack = self.stack();
        if stack.push_numeral(text) == 0 {
            return None;
        }
        let v = if stack.is_integer(-1) {
            Value::Int(stack.to_integer(-1).expect("an integer"))
        } else {
            Value::Float(stack.to_number(-1).expect("a number"))
        };
        let _ = stack.set_top(-2);
        Some(v)
    }

    /// The global table.
    pub fn globals(&'s self) -> Table<'s> {
        let _ = self.stack().raw_get_index(REGISTRY_INDEX, RIDX_GLOBALS);
        Table::top(self)
    }

    /// A new empty table.
    pub fn create_table(&'s self) -> Table<'s> {
        self.stack().create_table(0, 0);
        Table::top(self)
    }

    /// A string with these bytes (any bytes, not only UTF-8).
    pub fn create_string(&'s self, bytes: impl AsRef<[u8]>) -> Str<'s> {
        self.stack().push_bytes(bytes.as_ref());
        Str::top(self)
    }

    /// A function scripts can call that runs `f` ([`HostFunction`]).
    pub fn create_function(&'s self, f: impl HostFunction) -> Function<'s> {
        crate::host::push_function(&mut self.stack(), Rc::new(f));
        Function::top(self)
    }

    /// A userdata holding `value`, with no class: scripts can pass it
    /// around but not call methods on it. See [`Class`](crate::Class) for
    /// userdata with methods.
    pub fn create_userdata<T: 'static>(&'s self, value: T) -> Userdata<'s> {
        self.stack().push_userdata(value);
        Userdata::top(self)
    }

    /// Convert a Rust value to a script value.
    pub fn value(&'s self, v: impl IntoValue<'s>) -> Result<Value<'s>, Error> {
        v.push_onto(self)?;
        Ok(self.top())
    }

    /// Compile (or decode) `chunk` and give its main function, which runs
    /// the chunk when called.
    pub fn load(&'s self, chunk: Chunk<'_>) -> Result<Function<'s>, Error> {
        self.load_with(&chunk, 0)
    }

    /// Link a chunk [`Compiler`](crate::Compiler) compiled, perhaps on
    /// another thread, and give its main function.
    pub fn load_compiled(&'s self, chunk: &crate::UnlinkedChunk) -> Function<'s> {
        self.stack().push_compiled(chunk);
        Function::top(self)
    }

    /// [`Scope::load`], with `env` as the chunk's `_ENV` instead of the
    /// global table: usually a table, but any value will do, as for `load`.
    pub fn load_in(
        &'s self,
        chunk: Chunk<'_>,
        env: impl IntoValue<'s>,
    ) -> Result<Function<'s>, Error> {
        env.push_onto(self)?;
        let env = self.stack_ref().top();
        self.load_with(&chunk, env)
    }

    fn load_with(&'s self, chunk: &Chunk<'_>, env: i32) -> Result<Function<'s>, Error> {
        let mut stack = self.stack();
        match stack.load_chunk(chunk, env) {
            Status::Ok => {
                drop(stack);
                Ok(Function::top(self))
            }
            _ => {
                let message =
                    String::from_utf8_lossy(stack.string_at(-1).unwrap_or_default()).into_owned();
                let _ = stack.set_top(-2);
                Err(Error::syntax(message))
            }
        }
    }

    /// [`Isolate::before_exit`](crate::Isolate::before_exit), from inside
    /// the isolate (a library that keeps buffers registers its flush when
    /// it opens).
    pub fn before_exit(&self, hook: impl Fn() + 'static) {
        self.stack().before_exit(hook);
    }

    /// Make `module` what `require(name)` returns, as if a searcher had
    /// loaded it (`package.loaded[name] = module`).
    pub fn register_module(&'s self, name: &str, module: Table<'s>) {
        let mut stack = self.stack();
        let r: Result<(), Raised> = (|| {
            if stack.raw_get_field(REGISTRY_INDEX, b"_LOADED")? != Type::Table {
                stack.set_top(-2)?;
                stack.create_table(0, 1);
                stack.push_copy(-1);
                stack.raw_set_field(REGISTRY_INDEX, b"_LOADED")?;
            }
            stack.push_copy(module.slot());
            stack.raw_set_field(-2, name.as_bytes())?;
            stack.set_top(-2)
        })();
        debug_assert!(r.is_ok(), "registering a module raised {r:?}");
    }
}
