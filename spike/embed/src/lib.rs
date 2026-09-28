//! Prototype of the public embedding API over the own-arena VM (ADR-2).
//!
//! Two kinds of handle:
//!
//! - [`Root`]: a rooted, `'static` handle (a registry slot). Embedders keep
//!   these in their own structs across calls. Dropping one unroots it.
//! - [`Val<'cx>`]: a raw, unrooted value that borrows the [`Ctx`] it came
//!   from. Cheap (a `Value` and nothing else).
//!
//! The rule that keeps raw values safe without `unsafe`:
//!
//! - Operations that allocate but never collect take `&self` (the collector
//!   only runs in the interpreter loop, never inside a native call).
//! - Operations that can collect (running Lua) take `&mut self`.
//!
//! So the borrow checker rejects any `Val` that is still in use after a call
//! back into Lua. Root it first, or re-read it afterwards.
//!
//! A native function that calls back into Lua and keeps a value across the
//! call has to root it:
//!
//! ```
//! use spike_embed::{Ctx, R, Val};
//!
//! fn wrap<'a>(cx: &'a mut Ctx<'_>) -> R<Val<'a>> {
//!     let out = cx.new_table();
//!     let out = cx.root(out); // survives the call below
//!     let f = cx.arg(0);
//!     let call = cx.push_call(f, &[Val::int(1)]); // arguments move onto the Lua stack
//!     let r = cx.finish(call)?.as_int(); // may collect
//!     let out = cx.get_root(&out);
//!     cx.set(out, Val::int(1), Val::int(r.unwrap_or(0)))?;
//!     Ok(out)
//! }
//! ```
//!
//! Using a raw `Val` after the call does not compile:
//!
//! ```compile_fail,E0502
//! use spike_embed::{Ctx, R, Val};
//!
//! fn wrap<'a>(cx: &'a mut Ctx<'_>) -> R<Val<'a>> {
//!     let out = cx.new_table(); // not rooted
//!     let f = cx.arg(0);
//!     let call = cx.push_call(f, &[]);
//!     cx.finish(call)?; // error[E0502]: `out` still borrows `cx`
//!     cx.set(out, Val::int(1), Val::int(2))?;
//!     Ok(out)
//! }
//! ```
//!
//! Neither do calls whose arguments are raw values (they must be pushed first):
//!
//! ```compile_fail,E0502
//! use spike_embed::{Ctx, R, Val};
//!
//! fn twice<'a>(cx: &'a mut Ctx<'_>) -> R<Val<'a>> {
//!     let f = cx.arg(0);
//!     let a = cx.arg(1);
//!     let call = cx.push_call(f, &[a]);
//!     cx.finish(call)?;
//!     let call = cx.push_call(f, &[a]); // error[E0502]: `f` and `a` were taken before the call
//!     cx.finish(call)
//! }
//! ```
//!
//! And raw values can't escape to the embedder:
//!
//! ```compile_fail
//! let mut lua = spike_embed::Lua::new();
//! let t = lua.with(|cx| cx.new_table()); // error: lifetime may not live long enough
//! ```
//!
//! whereas rooted handles can:
//!
//! ```
//! let mut lua = spike_embed::Lua::new();
//! let t = lua.with(|cx| {
//!     let t = cx.new_table();
//!     cx.root(t)
//! });
//! lua.gc();
//! lua.with(|cx| assert!(cx.get_root(&t).is_table()));
//! ```

use spike_common::Proto;
use spike_vm_arena::{Dispatch, Key, NativeCall, TabRef, Value, Vm};
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;

pub use spike_common::VmError;
pub type R<T> = Result<T, VmError>;

/// Registry bookkeeping, shared by the `Lua` value and every native wrapper.
#[derive(Debug, Default)]
struct Registry {
    free: RefCell<Vec<u32>>,
    /// Slots whose `Root` was dropped; reclaimed at the next safe point.
    drops: Rc<RefCell<Vec<u32>>>,
}

impl Registry {
    fn reclaim(&self, vm: &mut Vm) {
        let mut drops = self.drops.borrow_mut();
        let mut free = self.free.borrow_mut();
        for slot in drops.drain(..) {
            vm.roots[slot as usize] = Value::Nil;
            free.push(slot);
        }
    }
}

/// A rooted, `'static` handle to a Lua value. Unroots itself on drop.
#[derive(Debug)]
pub struct Root {
    slot: u32,
    drops: Rc<RefCell<Vec<u32>>>,
}

impl Drop for Root {
    fn drop(&mut self) {
        self.drops.borrow_mut().push(self.slot);
    }
}

/// A raw value, valid while the `Ctx` it came from is not used mutably.
#[derive(Clone, Copy, Debug)]
pub struct Val<'cx> {
    raw: Value,
    _cx: PhantomData<&'cx ()>,
}

impl<'cx> Val<'cx> {
    fn new(raw: Value) -> Self {
        Val {
            raw,
            _cx: PhantomData,
        }
    }
    pub fn nil() -> Self {
        Val::new(Value::Nil)
    }
    pub fn int(i: i64) -> Self {
        Val::new(Value::Int(i))
    }
    pub fn as_int(self) -> Option<i64> {
        match self.raw {
            Value::Int(i) => Some(i),
            _ => None,
        }
    }
    pub fn is_nil(self) -> bool {
        self.raw.is_nil()
    }
    pub fn is_table(self) -> bool {
        matches!(self.raw, Value::Table(_))
    }
    /// The underlying value, for tests that inspect the heap.
    #[doc(hidden)]
    pub fn raw(self) -> Value {
        self.raw
    }
}

/// A call whose function and arguments are already on the Lua stack.
#[derive(Debug)]
#[must_use = "run it with Ctx::finish"]
pub struct PendingCall {
    slot: usize,
    nargs: u8,
}

/// The context native functions (and `Lua::with`) work through.
#[derive(Debug)]
pub struct Ctx<'vm> {
    vm: RefCell<&'vm mut Vm>,
    reg: Rc<Registry>,
    args: usize,
    nargs: u8,
    top: Cell<usize>,
}

impl<'vm> Ctx<'vm> {
    fn new(vm: &'vm mut Vm, reg: Rc<Registry>, call: NativeCall) -> Self {
        reg.reclaim(vm);
        Ctx {
            vm: RefCell::new(vm),
            reg,
            args: call.args,
            nargs: call.nargs,
            top: Cell::new(call.top),
        }
    }

    /// Argument `i` of the current native call (nil if absent).
    pub fn arg(&self, i: usize) -> Val<'_> {
        if i >= self.nargs as usize {
            return Val::nil();
        }
        Val::new(self.vm.borrow().stack[self.args + i])
    }

    pub fn nargs(&self) -> usize {
        self.nargs as usize
    }

    pub fn new_table(&self) -> Val<'_> {
        Val::new(Value::Table(self.vm.borrow_mut().heap.new_table()))
    }

    pub fn str(&self, s: &str) -> Val<'_> {
        Val::new(Value::Str(self.vm.borrow_mut().heap.new_str(s.as_bytes())))
    }

    fn table(t: Val<'_>) -> R<TabRef> {
        match t.raw {
            Value::Table(t) => Ok(t),
            _ => Err("attempt to index a non-table value".into()),
        }
    }

    pub fn get(&self, t: Val<'_>, k: Val<'_>) -> R<Val<'_>> {
        let t = Self::table(t)?;
        let mut vm = self.vm.borrow_mut();
        Ok(match vm.heap.key(k.raw) {
            Some(k) => Val::new(vm.heap.get(t, k)),
            None => Val::nil(),
        })
    }

    pub fn set(&self, t: Val<'_>, k: Val<'_>, v: Val<'_>) -> R<()> {
        let t = Self::table(t)?;
        let mut vm = self.vm.borrow_mut();
        let k: Key = vm.heap.key(k.raw).ok_or("invalid table key")?;
        vm.heap.set(t, k, v.raw);
        Ok(())
    }

    pub fn len(&self, t: Val<'_>) -> R<i64> {
        Ok(self.vm.borrow().heap.len(Self::table(t)?))
    }

    pub fn to_string(&self, v: Val<'_>) -> Option<String> {
        match v.raw {
            Value::Str(s) => {
                Some(String::from_utf8_lossy(self.vm.borrow().heap.str_bytes(s)).into())
            }
            _ => None,
        }
    }

    /// Root a value so it survives calls back into Lua and can leave the `Ctx`.
    pub fn root(&self, v: Val<'_>) -> Root {
        let mut vm = self.vm.borrow_mut();
        let slot = match self.reg.free.borrow_mut().pop() {
            Some(s) => {
                vm.roots[s as usize] = v.raw;
                s
            }
            None => {
                vm.roots.push(v.raw);
                (vm.roots.len() - 1) as u32
            }
        };
        Root {
            slot,
            drops: self.reg.drops.clone(),
        }
    }

    pub fn get_root(&self, r: &Root) -> Val<'_> {
        assert!(
            Rc::ptr_eq(&r.drops, &self.reg.drops),
            "Root used with a different Lua instance"
        );
        Val::new(self.vm.borrow().roots[r.slot as usize])
    }

    /// Push `f(args...)` onto the Lua stack. Once pushed, the function and
    /// arguments are rooted by the stack, so no borrow is kept.
    pub fn push_call(&self, f: Val<'_>, args: &[Val<'_>]) -> PendingCall {
        let slot = self.top.get();
        let mut vm = self.vm.borrow_mut();
        vm.ensure_stack(slot + 1 + args.len());
        vm.stack[slot] = f.raw;
        for (i, a) in args.iter().enumerate() {
            vm.stack[slot + 1 + i] = a.raw;
        }
        self.top.set(slot + 1 + args.len());
        PendingCall {
            slot,
            nargs: args.len() as u8,
        }
    }

    /// Run a pushed call. Runs Lua, so the collector may run: every `Val`
    /// taken before this point is dead (the borrow checker enforces it).
    pub fn finish(&mut self, call: PendingCall) -> R<Val<'_>> {
        let vm = self.vm.get_mut();
        let v = vm.call_value(call.slot, call.nargs)?;
        self.top.set(call.slot);
        Ok(Val::new(v))
    }

    /// The result of the last `finish`, readable alongside other `&self`
    /// calls. (`finish`'s own return value keeps `self` mutably borrowed for
    /// as long as it lives, a known limit of Rust borrows, so it's only good
    /// for immediate use such as `cx.finish(c)?.as_int()`.)
    pub fn result(&self) -> Val<'_> {
        Val::new(self.vm.borrow().stack[self.top.get()])
    }
}

/// Converts a Lua result into an owned Rust value.
pub trait FromLua: Sized {
    fn from_lua(cx: &Ctx<'_>, v: Val<'_>) -> R<Self>;
}

impl FromLua for i64 {
    fn from_lua(_: &Ctx<'_>, v: Val<'_>) -> R<Self> {
        v.as_int().ok_or_else(|| "expected an integer".into())
    }
}

impl FromLua for Root {
    fn from_lua(cx: &Ctx<'_>, v: Val<'_>) -> R<Self> {
        Ok(cx.root(v))
    }
}

impl FromLua for String {
    fn from_lua(cx: &Ctx<'_>, v: Val<'_>) -> R<Self> {
        cx.to_string(v).ok_or_else(|| "expected a string".into())
    }
}

/// An owned argument for [`Lua::call`].
#[derive(Debug)]
pub enum Arg<'a> {
    Int(i64),
    Str(&'a str),
    Root(&'a Root),
}

impl From<i64> for Arg<'_> {
    fn from(i: i64) -> Self {
        Arg::Int(i)
    }
}
impl<'a> From<&'a str> for Arg<'a> {
    fn from(s: &'a str) -> Self {
        Arg::Str(s)
    }
}
impl<'a> From<&'a Root> for Arg<'a> {
    fn from(r: &'a Root) -> Self {
        Arg::Root(r)
    }
}

/// The embedder's handle: owns the VM.
#[derive(Debug)]
pub struct Lua {
    vm: Vm,
    reg: Rc<Registry>,
}

impl Default for Lua {
    fn default() -> Self {
        Self::new()
    }
}

impl Lua {
    pub fn new() -> Self {
        Lua {
            vm: Vm::new(Dispatch::Super),
            reg: Rc::new(Registry::default()),
        }
    }

    /// Work with raw values. Nothing borrowed from the context can escape the
    /// closure; return owned data or `Root`s.
    pub fn with<T>(&mut self, f: impl FnOnce(&mut Ctx<'_>) -> T) -> T {
        let top = self.vm.frame_top();
        let mut cx = Ctx::new(
            &mut self.vm,
            self.reg.clone(),
            NativeCall {
                args: top,
                nargs: 0,
                top,
            },
        );
        f(&mut cx)
    }

    /// Load a (pre-assembled) chunk; returns its main function.
    pub fn load(&mut self, p: &Proto) -> Root {
        let id = self.vm.prepare(p);
        let f = self.vm.new_main_closure(id);
        self.with(|cx| cx.root(Val::new(f)))
    }

    /// Wrap a Rust function as a Lua function value.
    pub fn create_function<F>(&mut self, f: F) -> Root
    where
        F: for<'a, 'b> Fn(&'a mut Ctx<'b>) -> R<Val<'a>> + 'static,
    {
        let reg = self.reg.clone();
        let native = self.vm.register_native(Rc::new(move |vm: &mut Vm, call| {
            let mut cx = Ctx::new(vm, reg.clone(), call);
            Ok(f(&mut cx)?.raw)
        }));
        self.with(|cx| cx.root(Val::new(native)))
    }

    /// Call a Lua function with owned arguments.
    pub fn call<T: FromLua>(&mut self, f: &Root, args: &[Arg<'_>]) -> R<T> {
        self.with(|cx| {
            let args: Vec<Val<'_>> = args
                .iter()
                .map(|a| match a {
                    Arg::Int(i) => Val::int(*i),
                    Arg::Str(s) => cx.str(s),
                    Arg::Root(r) => cx.get_root(r),
                })
                .collect();
            let call = cx.push_call(cx.get_root(f), &args);
            cx.finish(call)?;
            T::from_lua(cx, cx.result())
        })
    }

    /// Run a full collection.
    pub fn gc(&mut self) {
        self.reg.reclaim(&mut self.vm);
        self.vm.full_gc();
    }

    /// Collect in tiny steps on every allocation (for tests).
    pub fn set_gc_stress(&mut self) {
        self.vm.heap.set_stress();
    }

    /// Root a raw internal value (tests only).
    #[doc(hidden)]
    pub fn root_raw(&mut self, v: Value) -> Root {
        self.with(|cx| cx.root(Val::new(v)))
    }

    /// The internal layer, for tests that inspect the heap.
    #[doc(hidden)]
    pub fn vm(&mut self) -> &mut Vm {
        &mut self.vm
    }
}
