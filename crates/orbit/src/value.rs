//! Script values and the scoped handles to them.

use std::cell::{Ref, RefMut};

use std::fmt;

use orbit_runtime::object::{self as raw, Gc, number};
use orbit_runtime::vm::{CoStatus, Meta, MetaField, NativeCtx};

use crate::class::Slot;
use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::Scope;

/// Any script value. Handles ([`Str`], [`Table`], ...) live as long as the
/// scope they came from.
#[derive(Clone, Copy)]
pub enum Value<'s> {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Str<'s>),
    Table(Table<'s>),
    Function(Function<'s>),
    Thread(Thread<'s>),
    Userdata(Userdata<'s>),
    /// An address a C module made (`lua_pushlightuserdata`), opaque here.
    LightUserdata(u64),
}

macro_rules! handle {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy)]
        pub struct $name<'s> {
            s: &'s Scope<'s>,
            v: raw::Value,
        }

        impl<'s> $name<'s> {
            /// A handle to `v`, which must be in the scope's window and of
            /// this kind.
            pub(crate) fn from_raw(s: &'s Scope<'s>, v: raw::Value) -> $name<'s> {
                $name { s, v }
            }

            #[allow(dead_code, reason = "not every kind of handle needs it")]
            pub(crate) fn raw(&self) -> raw::Value {
                self.v
            }

            /// This handle as a [`Value`].
            pub fn to_value(self) -> Value<'s> {
                Value::$name(self)
            }

            /// Keep the value alive past the scope.
            pub fn pin(&self) -> Pinned<$name<'static>> {
                self.s.pin_raw(self.v)
            }
        }

        impl fmt::Debug for $name<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.v)
            }
        }

        impl<'s> From<$name<'s>> for Value<'s> {
            fn from(h: $name<'s>) -> Value<'s> {
                Value::$name(h)
            }
        }
    };
}

handle!(
    /// A string, read in place. Strings hold any bytes.
    Str
);
handle!(
    /// A table.
    Table
);
handle!(
    /// A function: a script function or a host function.
    Function
);
handle!(
    /// A coroutine.
    Thread
);
handle!(
    /// A host object inside the script ([`Class`](crate::Class)).
    Userdata
);

impl<'s> Value<'s> {
    pub(crate) fn from_raw(s: &'s Scope<'s>, v: raw::Value) -> Value<'s> {
        match v {
            raw::Value::Nil => Value::Nil,
            raw::Value::Bool(b) => Value::Bool(b),
            raw::Value::Int(i) => Value::Int(i),
            raw::Value::Float(f) => Value::Float(f),
            raw::Value::Str(_) => Value::Str(Str::from_raw(s, v)),
            raw::Value::Table(_) => Value::Table(Table::from_raw(s, v)),
            raw::Value::Closure(_) | raw::Value::Native(_) => {
                Value::Function(Function::from_raw(s, v))
            }
            raw::Value::UserData(_) => Value::Userdata(Userdata::from_raw(s, v)),
            raw::Value::Thread(_) => Value::Thread(Thread::from_raw(s, v)),
            raw::Value::LightUserData(p) => Value::LightUserdata(p.get()),
        }
    }

    pub(crate) fn raw(&self) -> raw::Value {
        match *self {
            Value::Nil => raw::Value::Nil,
            Value::Bool(b) => raw::Value::Bool(b),
            Value::Int(i) => raw::Value::Int(i),
            Value::Float(f) => raw::Value::Float(f),
            Value::Str(h) => h.v,
            Value::Table(h) => h.v,
            Value::Function(h) => h.v,
            Value::Thread(h) => h.v,
            Value::Userdata(h) => h.v,
            Value::LightUserdata(p) => raw::Value::LightUserData(raw::LightUserData::new(p)),
        }
    }

    /// The scope of a handle (`None` for nil, booleans, numbers and light
    /// userdata, which need none).
    fn scope(&self) -> Option<&'s Scope<'s>> {
        match *self {
            Value::Str(h) => Some(h.s),
            Value::Table(h) => Some(h.s),
            Value::Function(h) => Some(h.s),
            Value::Thread(h) => Some(h.s),
            Value::Userdata(h) => Some(h.s),
            _ => None,
        }
    }

    /// The basic type name, as `type` returns it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::LightUserdata(_) => "userdata",
            v => v.raw().type_name(),
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Whether the value counts as true in a condition (anything but nil
    /// and false).
    pub fn truthy(&self) -> bool {
        !self.raw().is_falsy()
    }

    /// Whether two values are the same without calling `__eq` (`rawequal`).
    pub fn raw_eq(&self, other: &Value<'s>) -> bool {
        match self.scope().or(other.scope()) {
            Some(s) => raw::value::raw_eq(&s.ctx().vm.heap, self.raw(), other.raw()),
            None => self.raw().same(other.raw()) || self.raw_num_eq(other),
        }
    }

    fn raw_num_eq(&self, other: &Value<'_>) -> bool {
        match (*self, *other) {
            (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i)) => {
                number::int_eq_float(i, f)
            }
            (Value::Float(a), Value::Float(b)) => a == b,
            _ => false,
        }
    }

    /// The text `tostring` gives: `__tostring` and `__name` are honoured.
    pub fn to_display(&self) -> Result<Vec<u8>, Error> {
        let Some(s) = self.scope() else {
            let mut out = String::new();
            return Ok(match self {
                Value::Nil => b"nil".to_vec(),
                Value::Bool(b) => b.to_string().into_bytes(),
                Value::LightUserdata(p) => format!("userdata: {p:#x}").into_bytes(),
                v => {
                    number::fmt_number(&mut out, v.raw());
                    out.into_bytes()
                }
            });
        };
        let v = self.raw();
        let mut ctx = s.ctx();
        let mm = ctx.metamethod(v, MetaField::ToString);
        if mm.is_nil() {
            let mut out = Vec::new();
            ctx.display(v, &mut out);
            return Ok(out);
        }
        let at = ctx.len();
        ctx.push(mm);
        ctx.push(v);
        let r = ctx.call(1, None);
        let out = match r {
            Ok(n) => match (n > 0).then(|| ctx.arg(at)) {
                Some(raw::Value::Str(st)) => Ok(ctx.bytes(st).to_vec()),
                _ => Err(Error::runtime("'__tostring' must return a string")),
            },
            Err(e) => Err(s.lua_error(&mut ctx, e)),
        };
        ctx.truncate(at);
        out
    }

    /// The value's metatable: a table's or userdata's own, or the one
    /// shared by its type (strings have one).
    pub fn metatable(&self) -> Option<Table<'s>> {
        let s = self.scope()?;
        let mt = s.ctx().metatable(self.raw())?;
        Some(Table::from_raw(s, s.root(raw::Value::Table(mt)).raw()))
    }

    /// Convert to a Rust value ([`FromValue`]).
    pub fn get<T: FromValue<'s>>(self) -> Result<T, Error> {
        let name = self.type_name();
        T::from_value(self).map_err(|e| Error::runtime(e.message(name)))
    }

    /// Keep the value alive past the scope. Values that are not handles
    /// need no pin, but can have one, so a host can store any value alike.
    pub fn pin(&self) -> Option<Pinned<Value<'static>>> {
        self.scope().map(|s| s.pin_raw(self.raw()))
    }

    pub fn as_str(self) -> Option<Str<'s>> {
        match self {
            Value::Str(h) => Some(h),
            _ => None,
        }
    }

    pub fn as_table(self) -> Option<Table<'s>> {
        match self {
            Value::Table(h) => Some(h),
            _ => None,
        }
    }

    pub fn as_function(self) -> Option<Function<'s>> {
        match self {
            Value::Function(h) => Some(h),
            _ => None,
        }
    }

    pub fn as_thread(self) -> Option<Thread<'s>> {
        match self {
            Value::Thread(h) => Some(h),
            _ => None,
        }
    }

    pub fn as_userdata(self) -> Option<Userdata<'s>> {
        match self {
            Value::Userdata(h) => Some(h),
            _ => None,
        }
    }
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("Nil"),
            Value::Bool(b) => write!(f, "Bool({b})"),
            Value::Int(i) => write!(f, "Int({i})"),
            Value::Float(x) => write!(f, "Float({x})"),
            Value::Str(h) => h.fmt(f),
            Value::Table(h) => h.fmt(f),
            Value::Function(h) => h.fmt(f),
            Value::Thread(h) => h.fmt(f),
            Value::Userdata(h) => h.fmt(f),
            Value::LightUserdata(p) => write!(f, "LightUserdata({p:#x})"),
        }
    }
}

// ---- strings ----------------------------------------------------------------

impl<'s> Str<'s> {
    fn gc(&self) -> Gc<raw::LuaStr> {
        let raw::Value::Str(s) = self.v else {
            unreachable!("a Str handle holds a string")
        };
        s
    }

    /// The bytes, borrowed in place. While the borrow lives, other
    /// operations on the scope panic; copy with [`Str::to_bytes`] to keep
    /// them.
    pub fn as_bytes(&self) -> Ref<'_, [u8]> {
        let st = self.gc();
        Ref::map(self.s.ctx_ref(), |ctx| ctx.bytes(st))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }

    /// The text, if the bytes are UTF-8.
    pub fn to_str(&self) -> Result<String, Error> {
        String::from_utf8(self.to_bytes()).map_err(|_| Error::runtime("string is not valid UTF-8"))
    }

    /// The length in bytes.
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---- tables -----------------------------------------------------------------

impl<'s> Table<'s> {
    fn gc(&self) -> Gc<raw::Table> {
        let raw::Value::Table(t) = self.v else {
            unreachable!("a Table handle holds a table")
        };
        t
    }

    /// `t[key]`, with `__index`.
    pub fn get<V: FromValue<'s>>(&self, key: impl IntoValue<'s>) -> Result<V, Error> {
        let k = self.s.value(key)?.raw();
        let mut ctx = self.s.ctx();
        let m = ctx.index(self.v, k);
        let v = self.s.finish_meta(&mut ctx, m)?;
        drop(ctx);
        self.s.root(v).get()
    }

    /// `t[key] = value`, with `__newindex`.
    pub fn set(&self, key: impl IntoValue<'s>, value: impl IntoValue<'s>) -> Result<(), Error> {
        let k = self.s.value(key)?.raw();
        let v = self.s.value(value)?.raw();
        let mut ctx = self.s.ctx();
        let m = ctx.set_index(self.v, k, v).map(|m| match m {
            Meta::Done(()) => Meta::Done(raw::Value::Nil),
            Meta::Call { f, nargs } => Meta::Call { f, nargs },
        });
        self.s.finish_meta(&mut ctx, m).map(drop)
    }

    /// `t[key]` without metamethods (`rawget`).
    pub fn raw_get<V: FromValue<'s>>(&self, key: impl IntoValue<'s>) -> Result<V, Error> {
        let k = self.s.value(key)?.raw();
        let v = raw::table::get(&self.s.ctx().vm.heap, self.gc(), k);
        self.s.root(v).get()
    }

    /// `t[key] = value` without metamethods (`rawset`).
    pub fn raw_set(&self, key: impl IntoValue<'s>, value: impl IntoValue<'s>) -> Result<(), Error> {
        let k = self.s.value(key)?.raw();
        let v = self.s.value(value)?.raw();
        let mut ctx = self.s.ctx();
        raw_store(&mut ctx, self.gc(), k, v)
    }

    /// `#t`, with `__len`.
    #[allow(
        clippy::len_without_is_empty,
        reason = "`#t` is a border, not a count, so `#t == 0` isn't emptiness"
    )]
    pub fn len(&self) -> Result<i64, Error> {
        let mut ctx = self.s.ctx();
        let m = ctx.length(self.v);
        let v = self.s.finish_meta(&mut ctx, m)?;
        drop(ctx);
        self.s.root(v).get()
    }

    /// `#t` without metamethods (`rawlen`).
    pub fn raw_len(&self) -> usize {
        let ctx = self.s.ctx();
        ctx.vm.heap.get(self.gc()).len(&ctx.vm.heap) as usize
    }

    /// Append `value` after the last element (`t[#t + 1] = value`, raw).
    pub fn push(&self, value: impl IntoValue<'s>) -> Result<(), Error> {
        let n = self.raw_len() as i64;
        self.raw_set(n + 1, value)
    }

    pub fn metatable(&self) -> Option<Table<'s>> {
        let mt = self.s.ctx().metatable(self.v)?;
        Some(Table::from_raw(
            self.s,
            self.s.root(raw::Value::Table(mt)).raw(),
        ))
    }

    /// Set or clear the metatable. A metatable with `__gc` registers the
    /// table for finalization, as `setmetatable` does.
    pub fn set_metatable(&self, mt: Option<Table<'s>>) {
        let mut ctx = self.s.ctx();
        ctx.vm.set_metatable(self.v, mt.map(|t| t.gc()));
    }

    /// The table's pairs, without `__pairs`, in `next` order. Changing the
    /// table while iterating has `next`'s rules: assigning to existing
    /// fields is fine, adding new ones is not.
    pub fn pairs(&self) -> Pairs<'s> {
        Pairs {
            t: *self,
            key: raw::Value::Nil,
            done: false,
        }
    }
}

/// A raw table store, with the VM's error for a nil or NaN key.
fn raw_store(
    ctx: &mut NativeCtx<'_>,
    t: Gc<raw::Table>,
    k: raw::Value,
    v: raw::Value,
) -> Result<(), Error> {
    raw::table::set(&mut ctx.vm.heap, t, k, v).map_err(|e| Error::runtime(e.message()))
}

/// The iterator [`Table::pairs`] returns.
#[derive(Debug)]
pub struct Pairs<'s> {
    t: Table<'s>,
    key: raw::Value,
    done: bool,
}

impl<'s> Iterator for Pairs<'s> {
    type Item = Result<(Value<'s>, Value<'s>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let s = self.t.s;
        let next = {
            let ctx = s.ctx();
            ctx.vm.heap.get(self.t.gc()).next(&ctx.vm.heap, self.key)
        };
        match next {
            Ok(Some((k, v))) => {
                self.key = k;
                Some(Ok((s.root(k), s.root(v))))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(_) => {
                self.done = true;
                Some(Err(Error::runtime("invalid key to 'next'")))
            }
        }
    }
}

// ---- functions --------------------------------------------------------------

impl<'s> Function<'s> {
    /// Call the function and return all its results.
    pub fn call(&self, args: impl IntoValues<'s>) -> Result<Vec<Value<'s>>, Error> {
        self.call_with(args, Handler::None)
    }

    /// [`Function::call`], and an error carries a traceback of where it
    /// was raised ([`Error::traceback`]).
    pub fn call_traced(&self, args: impl IntoValues<'s>) -> Result<Vec<Value<'s>>, Error> {
        self.call_with(args, Handler::Traceback)
    }

    /// [`Function::call`] with a message handler, as `xpcall` has: on an
    /// error, `handler` runs with the error value on top of the frames
    /// that raised it (so it can inspect them), and what it returns
    /// becomes the error's value.
    pub fn call_handled(
        &self,
        args: impl IntoValues<'s>,
        handler: Function<'s>,
    ) -> Result<Vec<Value<'s>>, Error> {
        self.call_with(args, Handler::Function(handler.v))
    }

    fn call_with(
        &self,
        args: impl IntoValues<'s>,
        handler: Handler,
    ) -> Result<Vec<Value<'s>>, Error> {
        let s = self.s;
        let at = s.ctx().len();
        let slot = match handler {
            Handler::None => None,
            Handler::Traceback => {
                let mut ctx = s.ctx();
                let h = ctx.native("traceback handler", crate::host::traceback_handler);
                ctx.push(h);
                Some(at)
            }
            Handler::Function(h) => {
                s.ctx().push(h);
                Some(at)
            }
        };
        s.ctx().push(self.v);
        let nargs = match args.push_into(s) {
            Ok(n) => n,
            Err(e) => {
                s.ctx().truncate(at);
                return Err(e);
            }
        };
        let mut ctx = s.ctx();
        match ctx.call(nargs, slot) {
            Ok(n) => {
                drop(ctx);
                Ok(s.last(n))
            }
            Err(e) => {
                let tb = match handler {
                    Handler::Traceback => crate::host::take_traceback(&mut ctx),
                    _ => None,
                };
                let err = s.lua_error(&mut ctx, e);
                ctx.truncate(at);
                Err(err.with_traceback(tb))
            }
        }
    }
}

/// The message handler a call runs under.
#[derive(Clone, Copy)]
enum Handler {
    None,
    Traceback,
    Function(raw::Value),
}

// ---- threads ----------------------------------------------------------------

/// What [`Thread::status`] reports, as `coroutine.status` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadStatus {
    Suspended,
    Running,
    /// It resumed another coroutine and waits for it.
    Normal,
    Dead,
}

/// What a resume ended with.
#[derive(Debug)]
pub enum Resumed<'s> {
    /// The coroutine yielded these values.
    Yielded(Vec<Value<'s>>),
    /// The coroutine's function returned these values.
    Returned(Vec<Value<'s>>),
}

impl<'s> Thread<'s> {
    fn gc(&self) -> Gc<raw::Thread> {
        let raw::Value::Thread(t) = self.v else {
            unreachable!("a Thread handle holds a thread")
        };
        t
    }

    /// Resume the coroutine with `args` and wait until it yields, returns
    /// or fails.
    pub fn resume(&self, args: impl IntoValues<'s>) -> Result<Resumed<'s>, Error> {
        let s = self.s;
        let at = s.ctx().len();
        let n = args.push_into(s)?;
        let mut ctx = s.ctx();
        let args: Vec<raw::Value> = (at..at + n).map(|i| ctx.arg(i)).collect();
        let r = ctx.resume_nested(self.gc(), &args);
        ctx.truncate(at);
        match r {
            Ok((returned, vals)) => {
                for &v in &vals {
                    ctx.push(v);
                }
                drop(ctx);
                let vals = s.last(vals.len());
                Ok(if returned {
                    Resumed::Returned(vals)
                } else {
                    Resumed::Yielded(vals)
                })
            }
            Err(e) => {
                let e = orbit_runtime::vm::LuaError::new(e);
                Err(s.lua_error(&mut ctx, e))
            }
        }
    }

    pub fn status(&self) -> ThreadStatus {
        match self.s.ctx().thread_status(self.gc()) {
            CoStatus::Suspended => ThreadStatus::Suspended,
            CoStatus::Running => ThreadStatus::Running,
            CoStatus::Normal => ThreadStatus::Normal,
            CoStatus::Dead => ThreadStatus::Dead,
        }
    }
}

impl<'s> Scope<'s> {
    /// A coroutine that runs `f` when first resumed.
    pub fn create_thread(&'s self, f: Function<'s>) -> Thread<'s> {
        let t = self.ctx().new_thread(f.v);
        Thread::from_raw(self, self.root(raw::Value::Thread(t)).raw())
    }
}

// ---- userdata ---------------------------------------------------------------

impl<'s> Userdata<'s> {
    pub(crate) fn gc(&self) -> Gc<raw::UserData> {
        let raw::Value::UserData(u) = self.v else {
            unreachable!("a Userdata handle holds a userdata")
        };
        u
    }

    /// The host value, if it is a `T` this API stored
    /// ([`Scope::create_userdata`], [`Class`](crate::Class)). While the
    /// borrow lives, other operations on the scope panic.
    pub fn borrow<T: 'static>(&self) -> Result<Ref<'_, T>, Error> {
        let u = self.gc();
        Ref::filter_map(self.s.ctx_ref(), |ctx| {
            ctx.userdata_ref::<Slot<T>>(u).and_then(|s| s.0.as_ref())
        })
        .map_err(|_| Error::runtime(not_a::<T>()))
    }

    /// The host value, mutably, if it is a `T` this API stored.
    pub fn borrow_mut<T: 'static>(&self) -> Result<RefMut<'_, T>, Error> {
        let u = self.gc();
        RefMut::filter_map(self.s.ctx(), |ctx| {
            ctx.userdata_mut::<Slot<T>>(u).and_then(|s| s.0.as_mut())
        })
        .map_err(|_| Error::runtime(not_a::<T>()))
    }
}

/// Why a userdata borrow failed: another type, or a running method has the
/// object.
fn not_a<T>() -> String {
    format!(
        "userdata is not an available {}",
        std::any::type_name::<T>()
    )
}

// ---- conversions ------------------------------------------------------------

/// Why a value didn't convert ([`FromValue`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromValueError {
    /// It has the wrong type: this one was expected (`number`).
    Expected(&'static str),
    /// It has the right type but not a usable value.
    Invalid(&'static str),
}

impl FromValueError {
    /// The message, given the type name of the value that failed.
    pub(crate) fn message(self, got: &str) -> String {
        match self {
            FromValueError::Expected(want) => format!("{want} expected, got {got}"),
            FromValueError::Invalid(why) => why.to_owned(),
        }
    }
}

/// A Rust type a script value converts to.
pub trait FromValue<'s>: Sized {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError>;
}

/// A Rust type that converts to a script value.
pub trait IntoValue<'s> {
    /// The value; a new string or table is not yet rooted, so the caller
    /// roots it before allocating again.
    #[doc(hidden)]
    fn into_raw(self, s: &'s Scope<'s>) -> Result<raw::Value, Error>;
}

/// Values given to or returned from a call: `()`, one value, a tuple, or a
/// `Vec`.
pub trait IntoValues<'s> {
    /// Push the values on the scope's window; returns how many.
    #[doc(hidden)]
    fn push_into(self, s: &'s Scope<'s>) -> Result<usize, Error>;
}

impl<'s> FromValue<'s> for Value<'s> {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        Ok(v)
    }
}

impl<'s> FromValue<'s> for bool {
    /// Truthiness, as conditions use it.
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        Ok(v.truthy())
    }
}

impl<'s> FromValue<'s> for i64 {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        let n = match v {
            Value::Str(st) => number::str_to_number(&st.as_bytes()),
            v => Some(v.raw()),
        };
        match n {
            Some(raw::Value::Int(i)) => Ok(i),
            Some(raw::Value::Float(f)) => number::float_to_int(f).ok_or(FromValueError::Invalid(
                "number has no integer representation",
            )),
            _ => Err(FromValueError::Expected("number")),
        }
    }
}

impl<'s> FromValue<'s> for f64 {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        let n = match v {
            Value::Str(st) => number::str_to_number(&st.as_bytes()),
            v => Some(v.raw()),
        };
        n.and_then(|n| n.as_f64())
            .ok_or(FromValueError::Expected("number"))
    }
}

impl<'s> FromValue<'s> for Vec<u8> {
    /// A string's bytes, or a number's text, as `tostring` writes it.
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        match v {
            Value::Str(st) => Ok(st.to_bytes()),
            Value::Int(_) | Value::Float(_) => {
                let mut out = String::new();
                number::fmt_number(&mut out, v.raw());
                Ok(out.into_bytes())
            }
            _ => Err(FromValueError::Expected("string")),
        }
    }
}

impl<'s> FromValue<'s> for String {
    /// [`Vec<u8>`]'s conversion, for UTF-8 text.
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        String::from_utf8(Vec::<u8>::from_value(v)?)
            .map_err(|_| FromValueError::Invalid("string is not valid UTF-8"))
    }
}

impl<'s, T: FromValue<'s>> FromValue<'s> for Option<T> {
    /// `None` for nil.
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        match v {
            Value::Nil => Ok(None),
            v => T::from_value(v).map(Some),
        }
    }
}

macro_rules! from_handle {
    ($($t:ident => $as:ident, $name:literal);* $(;)?) => {$(
        impl<'s> FromValue<'s> for $t<'s> {
            fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
                v.$as().ok_or(FromValueError::Expected($name))
            }
        }

        impl<'s> IntoValue<'s> for $t<'s> {
            fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
                Ok(self.v)
            }
        }
    )*};
}

from_handle!(
    Str => as_str, "string";
    Table => as_table, "table";
    Function => as_function, "function";
    Thread => as_thread, "thread";
    Userdata => as_userdata, "userdata";
);

impl<'s> IntoValue<'s> for Value<'s> {
    fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(self.raw())
    }
}

impl<'s> IntoValue<'s> for &Value<'s> {
    fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(self.raw())
    }
}

impl<'s> IntoValue<'s> for bool {
    fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(raw::Value::Bool(self))
    }
}

macro_rules! into_int {
    ($($t:ty),*) => {$(
        impl<'s> IntoValue<'s> for $t {
            fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
                Ok(raw::Value::Int(i64::from(self)))
            }
        }
    )*};
}

into_int!(i8, i16, i32, i64, u8, u16, u32);

impl<'s> IntoValue<'s> for f64 {
    fn into_raw(self, _: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(raw::Value::Float(self))
    }
}

impl<'s> IntoValue<'s> for &str {
    fn into_raw(self, s: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(s.ctx().new_str(self.as_bytes()))
    }
}

impl<'s> IntoValue<'s> for String {
    fn into_raw(self, s: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(s.ctx().new_str_owned(self.into_bytes()))
    }
}

impl<'s> IntoValue<'s> for &[u8] {
    fn into_raw(self, s: &'s Scope<'s>) -> Result<raw::Value, Error> {
        Ok(s.ctx().new_str(self))
    }
}

impl<'s, T: IntoValue<'s>> IntoValue<'s> for Option<T> {
    /// nil for `None`.
    fn into_raw(self, s: &'s Scope<'s>) -> Result<raw::Value, Error> {
        match self {
            Some(v) => v.into_raw(s),
            None => Ok(raw::Value::Nil),
        }
    }
}

impl<'s> IntoValues<'s> for () {
    fn push_into(self, _: &'s Scope<'s>) -> Result<usize, Error> {
        Ok(0)
    }
}

impl<'s, T: IntoValue<'s>> IntoValues<'s> for T {
    fn push_into(self, s: &'s Scope<'s>) -> Result<usize, Error> {
        let v = self.into_raw(s)?;
        s.ctx().push(v);
        Ok(1)
    }
}

impl<'s, T: IntoValue<'s>> IntoValues<'s> for Vec<T> {
    fn push_into(self, s: &'s Scope<'s>) -> Result<usize, Error> {
        let n = self.len();
        for v in self {
            v.push_into(s)?;
        }
        Ok(n)
    }
}

macro_rules! into_values_tuple {
    ($($t:ident),+) => {
        impl<'s, $($t: IntoValue<'s>),+> IntoValues<'s> for ($($t,)+) {
            #[allow(non_snake_case)]
            fn push_into(self, s: &'s Scope<'s>) -> Result<usize, Error> {
                let ($($t,)+) = self;
                let mut n = 0;
                $( n += $t.push_into(s)?; )+
                Ok(n)
            }
        }
    };
}

into_values_tuple!(A);
into_values_tuple!(A, B);
into_values_tuple!(A, B, C);
into_values_tuple!(A, B, C, D);
into_values_tuple!(A, B, C, D, E);
into_values_tuple!(A, B, C, D, E, F);
into_values_tuple!(A, B, C, D, E, F, G);
into_values_tuple!(A, B, C, D, E, F, G, H);
