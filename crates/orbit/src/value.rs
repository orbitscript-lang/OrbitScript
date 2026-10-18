//! Script values and the scoped handles to them.

use std::cell::{Ref, RefMut};

use std::fmt;

use orbit_runtime::object::number;

use crate::error::Error;
use crate::pin::Pinned;
use crate::scope::Scope;
use crate::stack::{Continuation, Native, Outcome, Return, Stack, StackResult, Type};

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
            /// Its slot in the scope's window.
            idx: i32,
        }

        impl<'s> $name<'s> {
            /// The handle to slot `idx`, which must hold a value of this
            /// kind.
            pub(crate) fn at(s: &'s Scope<'s>, idx: i32) -> $name<'s> {
                $name { s, idx }
            }

            /// The handle to the value on top of the window.
            pub(crate) fn top(s: &'s Scope<'s>) -> $name<'s> {
                let idx = s.stack_ref().top();
                $name { s, idx }
            }

            #[allow(dead_code, reason = "not every kind of handle needs it")]
            pub(crate) fn slot(&self) -> i32 {
                self.idx
            }

            /// This handle as a [`Value`].
            pub fn to_value(self) -> Value<'s> {
                Value::$name(self)
            }

            /// Keep the value alive past the scope.
            pub fn pin(&self) -> Pinned<$name<'static>> {
                self.s.pin_slot(self.idx)
            }
        }

        impl fmt::Debug for $name<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(#{})", stringify!($name), self.idx)
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
    /// The value in slot `idx` of the scope's window: a handle to that
    /// slot, or the value itself for nil, booleans, numbers and light
    /// userdata.
    pub(crate) fn at(s: &'s Scope<'s>, idx: i32) -> Value<'s> {
        let stack = s.stack_ref();
        match stack.type_at(idx) {
            Type::None | Type::Nil => Value::Nil,
            Type::Boolean => Value::Bool(stack.to_boolean(idx)),
            Type::Number => match stack.is_integer(idx) {
                true => Value::Int(stack.to_integer(idx).expect("an integer")),
                false => Value::Float(stack.to_number(idx).expect("a number")),
            },
            Type::String => Value::Str(Str::at(s, idx)),
            Type::Table => Value::Table(Table::at(s, idx)),
            Type::Function => Value::Function(Function::at(s, idx)),
            Type::Thread => Value::Thread(Thread::at(s, idx)),
            Type::Userdata => Value::Userdata(Userdata::at(s, idx)),
            Type::LightUserdata => Value::LightUserdata(stack.to_userdata(idx) as usize as u64),
        }
    }

    /// Push the value on `stack`, the window of its scope.
    pub(crate) fn push(&self, stack: &mut Stack<'_>) {
        match *self {
            Value::Nil => stack.push_nil(),
            Value::Bool(b) => stack.push_boolean(b),
            Value::Int(i) => stack.push_integer(i),
            Value::Float(f) => stack.push_number(f),
            Value::LightUserdata(p) => stack.push_light_userdata(p as usize),
            v => stack.push_copy(v.slot().expect("a handle")),
        }
    }

    /// The slot of a handle (`None` for nil, booleans, numbers and light
    /// userdata, which need none).
    pub(crate) fn slot(&self) -> Option<i32> {
        match *self {
            Value::Str(h) => Some(h.idx),
            Value::Table(h) => Some(h.idx),
            Value::Function(h) => Some(h.idx),
            Value::Thread(h) => Some(h.idx),
            Value::Userdata(h) => Some(h.idx),
            _ => None,
        }
    }

    /// The scope of a handle.
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

    /// The address of a table, function, thread or userdata, as `%p`
    /// shows it (`None` for other values): the same while the object lives.
    pub fn address(&self) -> Option<u64> {
        let s = self.scope()?;
        s.stack_ref().identity(self.slot()?)
    }

    /// The basic type name, as `type` returns it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
            Value::Thread(_) => "thread",
            Value::Userdata(_) | Value::LightUserdata(_) => "userdata",
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Whether the value counts as true in a condition (anything but nil
    /// and false).
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Whether two values are the same without calling `__eq` (`rawequal`).
    pub fn raw_eq(&self, other: &Value<'s>) -> bool {
        match (self.slot(), other.slot()) {
            (Some(a), Some(b)) => {
                let s = self.scope().expect("a handle");
                s.stack_ref().raw_equal(a, b)
            }
            (None, None) => match (*self, *other) {
                (Value::Nil, Value::Nil) => true,
                (Value::Bool(a), Value::Bool(b)) => a == b,
                (Value::Int(a), Value::Int(b)) => a == b,
                (Value::Float(a), Value::Float(b)) => a == b,
                (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i)) => {
                    number::int_eq_float(i, f)
                }
                (Value::LightUserdata(a), Value::LightUserdata(b)) => a == b,
                _ => false,
            },
            _ => false,
        }
    }

    /// The text `tostring` gives: `__tostring` and `__name` are honoured.
    pub fn to_display(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        self.write_display(&mut out)?;
        Ok(out)
    }

    /// Append the text `tostring` gives to `out`, as [`to_display`] does
    /// without a buffer per value.
    ///
    /// [`to_display`]: Value::to_display
    pub fn write_display(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let (Some(s), Some(idx)) = (self.scope(), self.slot()) else {
            match self {
                Value::Nil => out.extend_from_slice(b"nil"),
                Value::Bool(b) => out.extend_from_slice(b.to_string().as_bytes()),
                Value::LightUserdata(p) => {
                    out.extend_from_slice(format!("userdata: {p:#x}").as_bytes())
                }
                v => {
                    v.write_text(out);
                }
            }
            return Ok(());
        };
        let mut stack = s.stack();
        let at = stack.top();
        let r = stack
            .to_display_string(idx)
            .map(|b| out.extend_from_slice(b));
        let r = r.map_err(|e| s.error_of(&mut stack, e));
        let _ = stack.set_top(at);
        r
    }

    /// Append a string's bytes, or a number's text as `tostring` writes it,
    /// to `out`: what converting to `Vec<u8>` gives, without the buffer.
    /// Other values append nothing and give `false`.
    pub fn write_text(&self, out: &mut Vec<u8>) -> bool {
        match self {
            Value::Str(st) => out.extend_from_slice(&st.as_bytes()),
            Value::Int(i) => number::push_int(out, *i),
            Value::Float(f) => {
                let mut text = String::new();
                number::fmt_number(&mut text, orbit_runtime::object::Value::Float(*f));
                out.extend_from_slice(text.as_bytes());
            }
            _ => return false,
        }
        true
    }

    /// The value's metatable: a table's or userdata's own, or the one
    /// shared by its type (strings have one).
    pub fn metatable(&self) -> Option<Table<'s>> {
        let s = self.scope()?;
        metatable_of(s, self.slot()?)
    }

    /// Convert to a Rust value ([`FromValue`]).
    pub fn get<T: FromValue<'s>>(self) -> Result<T, Error> {
        let name = self.type_name();
        T::from_value(self).map_err(|e| Error::runtime(e.message(name)))
    }

    /// Keep the value alive past the scope. Values that are not handles
    /// need no pin, but can have one, so a host can store any value alike.
    pub fn pin(&self) -> Option<Pinned<Value<'static>>> {
        Some(self.scope()?.pin_slot(self.slot()?))
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

/// The metatable of the value in slot `idx`.
fn metatable_of<'s>(s: &'s Scope<'s>, idx: i32) -> Option<Table<'s>> {
    let found = s.stack().get_metatable(idx);
    found.then(|| Table::top(s))
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
    /// The bytes, borrowed in place. While the borrow lives, other
    /// operations on the scope panic; copy with [`Str::to_bytes`] to keep
    /// them.
    pub fn as_bytes(&self) -> Ref<'_, [u8]> {
        let idx = self.idx;
        Ref::map(self.s.stack_ref(), |stack| {
            stack.string_at(idx).expect("a Str handle holds a string")
        })
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
    /// `t[key]`, with `__index`.
    pub fn get<V: FromValue<'s>>(&self, key: impl IntoValue<'s>) -> Result<V, Error> {
        key.push_onto(self.s)?;
        self.s.run(|st| st.get_table(self.idx))?;
        self.s.top().get()
    }

    /// `t[key] = value`, with `__newindex`.
    pub fn set(&self, key: impl IntoValue<'s>, value: impl IntoValue<'s>) -> Result<(), Error> {
        key.push_onto(self.s)?;
        value.push_onto(self.s)?;
        self.s.run(|st| st.set_table(self.idx))
    }

    /// `t[key]` without metamethods (`rawget`).
    pub fn raw_get<V: FromValue<'s>>(&self, key: impl IntoValue<'s>) -> Result<V, Error> {
        key.push_onto(self.s)?;
        self.s.run(|st| st.raw_get(self.idx))?;
        self.s.top().get()
    }

    /// `t[key] = value` without metamethods (`rawset`).
    pub fn raw_set(&self, key: impl IntoValue<'s>, value: impl IntoValue<'s>) -> Result<(), Error> {
        key.push_onto(self.s)?;
        value.push_onto(self.s)?;
        self.s.run(|st| st.raw_set(self.idx))
    }

    /// `#t`, with `__len`.
    #[allow(
        clippy::len_without_is_empty,
        reason = "`#t` is a border, not a count, so `#t == 0` isn't emptiness"
    )]
    pub fn len(&self) -> Result<i64, Error> {
        self.s.run(|st| st.len(self.idx))?;
        self.s.top().get()
    }

    /// `#t` without metamethods (`rawlen`).
    pub fn raw_len(&self) -> usize {
        self.s.stack_ref().raw_len(self.idx) as usize
    }

    /// Append `value` after the last element (`t[#t + 1] = value`, raw).
    pub fn push(&self, value: impl IntoValue<'s>) -> Result<(), Error> {
        let n = self.raw_len() as i64;
        self.raw_set(n + 1, value)
    }

    pub fn metatable(&self) -> Option<Table<'s>> {
        metatable_of(self.s, self.idx)
    }

    /// Set or clear the metatable. A metatable with `__gc` registers the
    /// table for finalization, as `setmetatable` does.
    pub fn set_metatable(&self, mt: Option<Table<'s>>) {
        let mut stack = self.s.stack();
        match mt {
            Some(mt) => stack.push_copy(mt.idx),
            None => stack.push_nil(),
        }
        let r = stack.set_metatable(self.idx);
        debug_assert!(r.is_ok(), "a table or nil is a metatable");
    }

    /// The table's pairs, without `__pairs`, in `next` order. Changing the
    /// table while iterating has `next`'s rules: assigning to existing
    /// fields is fine, adding new ones is not.
    pub fn pairs(&self) -> Pairs<'s> {
        Pairs {
            t: *self,
            key: None,
            done: false,
        }
    }
}

/// The iterator [`Table::pairs`] returns.
#[derive(Debug)]
pub struct Pairs<'s> {
    t: Table<'s>,
    /// The slot of the last key given.
    key: Option<i32>,
    done: bool,
}

impl<'s> Iterator for Pairs<'s> {
    type Item = Result<(Value<'s>, Value<'s>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let s = self.t.s;
        let mut stack = s.stack();
        match self.key {
            Some(k) => stack.push_copy(k),
            None => stack.push_nil(),
        }
        match stack.next(self.t.idx) {
            Ok(true) => {
                let top = stack.top();
                drop(stack);
                self.key = Some(top - 1);
                Some(Ok((s.at(top - 1), s.at(top))))
            }
            Ok(false) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(s.error_of(&mut stack, e)))
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
        self.call_with(args, Handler::Function(handler.idx))
    }

    fn call_with(
        &self,
        args: impl IntoValues<'s>,
        handler: Handler,
    ) -> Result<Vec<Value<'s>>, Error> {
        let s = self.s;
        let mut stack = s.stack();
        let at = stack.top();
        match handler {
            Handler::None => {}
            Handler::Traceback => stack.push_native::<TracebackHandler>(),
            Handler::Function(h) => stack.push_copy(h),
        }
        stack.push_copy(self.idx);
        drop(stack);
        let nargs = match args.push_into(s) {
            Ok(n) => n as i32,
            Err(e) => {
                let _ = s.stack().set_top(at);
                return Err(e);
            }
        };
        let mut stack = s.stack();
        let r = match handler {
            Handler::None => stack.call(nargs, crate::stack::MULTRET),
            _ => stack.call_handled(nargs, crate::stack::MULTRET, at + 1),
        };
        match r {
            Ok(()) => {
                let base = if matches!(handler, Handler::None) {
                    at
                } else {
                    at + 1
                };
                let n = (stack.top() - base) as usize;
                drop(stack);
                Ok(s.last(n))
            }
            Err(e) => {
                let tb = match handler {
                    Handler::Traceback => stack.extension(|p: &mut PendingTraceback| p.0.take()),
                    _ => None,
                };
                let err = s.error_of(&mut stack, e);
                let _ = stack.set_top(at);
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
    /// The handler function's slot.
    Function(i32),
}

/// The traceback [`TracebackHandler`] took, waiting for the caller.
#[derive(Default)]
struct PendingTraceback(Option<String>);

/// A message handler that notes a traceback and leaves the error as it is
/// ([`Function::call_traced`]).
struct TracebackHandler;

impl Native for TracebackHandler {
    const NAME: &'static str = "traceback handler";

    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        s.push_traceback(None, 1);
        let tb = String::from_utf8_lossy(s.string_at(-1).unwrap_or_default()).into_owned();
        s.set_top(-2)?;
        s.extension(|p: &mut PendingTraceback| p.0 = Some(tb));
        s.push_copy(1);
        Ok(Return::values(1))
    }
}

/// The end of a [`Call::tail_call`](crate::Call::tail_call): the call's
/// results, or its error, are the host function's.
pub(crate) struct TailCalled;

impl Continuation for TailCalled {
    fn resume(_: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return> {
        match outcome {
            Outcome::Ok(n) => Ok(Return::values(n)),
            Outcome::Err(e) => Err(e),
        }
    }
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
    /// Resume the coroutine with `args` and wait until it yields, returns
    /// or fails.
    pub fn resume(&self, args: impl IntoValues<'s>) -> Result<Resumed<'s>, Error> {
        let s = self.s;
        let n = args.push_into(s)?;
        let (returned, n) = s.run(|st| st.resume_here(self.idx, n))?;
        let vals = s.last(n);
        Ok(if returned {
            Resumed::Returned(vals)
        } else {
            Resumed::Yielded(vals)
        })
    }

    pub fn status(&self) -> ThreadStatus {
        let stack = self.s.stack_ref();
        let t = stack
            .to_thread(self.idx)
            .expect("a Thread handle holds a thread");
        stack.thread_status(t)
    }
}

impl<'s> Scope<'s> {
    /// A coroutine that runs `f` when first resumed.
    pub fn create_thread(&'s self, f: Function<'s>) -> Thread<'s> {
        self.stack().push_coroutine(f.idx);
        Thread::top(self)
    }
}

// ---- userdata ---------------------------------------------------------------

impl<'s> Userdata<'s> {
    /// The host value, if it is a `T` this API stored
    /// ([`Scope::create_userdata`], [`Class`](crate::Class)). While the
    /// borrow lives, other operations on the scope panic.
    pub fn borrow<T: 'static>(&self) -> Result<Ref<'_, T>, Error> {
        let idx = self.idx;
        Ref::filter_map(self.s.stack_ref(), |stack| {
            stack.userdata_in_place::<T>(idx)
        })
        .map_err(|_| Error::runtime(not_a::<T>()))
    }

    /// The host value, mutably, if it is a `T` this API stored.
    pub fn borrow_mut<T: 'static>(&self) -> Result<RefMut<'_, T>, Error> {
        let idx = self.idx;
        RefMut::filter_map(self.s.stack(), |stack| {
            stack.userdata_in_place_mut::<T>(idx)
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
    /// Push the value on the scope's window.
    #[doc(hidden)]
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error>;
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

/// A string as the number it spells, as arithmetic converts it.
fn numeral(st: Str<'_>) -> Option<Value<'_>> {
    let stack = st.s.stack_ref();
    match stack.to_integer(st.idx) {
        Some(i) => Some(Value::Int(i)),
        None => stack.to_number(st.idx).map(Value::Float),
    }
}

impl<'s> FromValue<'s> for i64 {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        let n = match v {
            Value::Str(st) => numeral(st),
            v => Some(v),
        };
        match n {
            Some(Value::Int(i)) => Ok(i),
            Some(Value::Float(f)) => number::float_to_int(f).ok_or(FromValueError::Invalid(
                "number has no integer representation",
            )),
            _ => Err(FromValueError::Expected("number")),
        }
    }
}

impl<'s> FromValue<'s> for f64 {
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        let n = match v {
            Value::Str(st) => numeral(st),
            v => Some(v),
        };
        match n {
            Some(Value::Int(i)) => Ok(i as f64),
            Some(Value::Float(f)) => Ok(f),
            _ => Err(FromValueError::Expected("number")),
        }
    }
}

impl<'s> FromValue<'s> for Vec<u8> {
    /// A string's bytes, or a number's text, as `tostring` writes it.
    fn from_value(v: Value<'s>) -> Result<Self, FromValueError> {
        match v {
            Value::Str(st) => Ok(st.to_bytes()),
            _ => {
                let mut out = Vec::new();
                match v.write_text(&mut out) {
                    true => Ok(out),
                    false => Err(FromValueError::Expected("string")),
                }
            }
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
            fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
                s.stack().push_copy(self.idx);
                Ok(())
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
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        self.push(&mut s.stack());
        Ok(())
    }
}

impl<'s> IntoValue<'s> for &Value<'s> {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        self.push(&mut s.stack());
        Ok(())
    }
}

impl<'s> IntoValue<'s> for bool {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        s.stack().push_boolean(self);
        Ok(())
    }
}

macro_rules! into_int {
    ($($t:ty),*) => {$(
        impl<'s> IntoValue<'s> for $t {
            fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
                s.stack().push_integer(i64::from(self));
                Ok(())
            }
        }
    )*};
}

into_int!(i8, i16, i32, i64, u8, u16, u32);

impl<'s> IntoValue<'s> for f64 {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        s.stack().push_number(self);
        Ok(())
    }
}

impl<'s> IntoValue<'s> for &str {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        s.stack().push_bytes(self.as_bytes());
        Ok(())
    }
}

impl<'s> IntoValue<'s> for String {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        s.stack().push_bytes(self.as_bytes());
        Ok(())
    }
}

impl<'s> IntoValue<'s> for &[u8] {
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        s.stack().push_bytes(self);
        Ok(())
    }
}

impl<'s, T: IntoValue<'s>> IntoValue<'s> for Option<T> {
    /// nil for `None`.
    fn push_onto(self, s: &'s Scope<'s>) -> Result<(), Error> {
        match self {
            Some(v) => v.push_onto(s),
            None => {
                s.stack().push_nil();
                Ok(())
            }
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
        self.push_onto(s)?;
        Ok(1)
    }
}

impl<'s, T: IntoValue<'s>> IntoValues<'s> for Vec<T> {
    fn push_into(self, s: &'s Scope<'s>) -> Result<usize, Error> {
        let n = self.len();
        for v in self {
            v.push_onto(s)?;
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
