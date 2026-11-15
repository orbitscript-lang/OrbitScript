//! Typed arguments and results for [`Native`](super::Native)s, so a native
//! can be an ordinary Rust function: `#[native] fn difftime(t2: i64, t1:
//! i64) -> f64`. The `#[native]` attribute (`orbit::native`) writes the
//! [`Native`](super::Native) impl that reads each parameter with [`Arg`] and
//! pushes the return value with [`Results`]; both are plain traits, so a
//! hand-written native can use them too:
//!
//! ```ignore
//! let (t2, t1) = (i64::arg(s, 1)?, i64::arg(s, 2)?);
//! (t2 as f64 - t1 as f64).push_results(s)
//! ```
//!
//! Everything here is generic and inlines to the calls a hand-written native
//! makes (`check_integer`, `push_number`, ...): no boxing, no dynamic
//! dispatch, no `Value` handles.

use super::{Native, Raised, Return, Stack, StackResult, Type};
pub use crate::object::number::Number;
use crate::object::{Gc, LuaStr, OrbitStr, OrbitString};

/// A type a native's parameter can have: how to read argument `i`
/// (1-based) as one, raising Lua's argument error when it can't.
///
/// Reading has two steps so a parameter can borrow from the stack (`&[u8]`,
/// `&str`): [`Arg::check`] runs first for every parameter and may change the
/// stack (a number argument becomes a string in place, as `lua_tolstring`
/// does), then [`Arg::get`] reads them all through one shared borrow.
/// Owned types do all their work in `check`.
pub trait Arg<'a>: Sized {
    /// What `check` learned, handed to `get`.
    type Checked;

    /// Check argument `i`, raising Lua's argument error.
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Self::Checked>;

    /// The checked argument.
    fn get(s: &'a Stack<'_>, i: i32, checked: Self::Checked) -> Self;

    /// [`Arg::check`] for an optional parameter: `None` when the argument
    /// is absent or nil. Types override it to test the type once.
    #[inline(always)]
    fn check_opt(s: &mut Stack<'_>, i: i32) -> StackResult<Option<Self::Checked>> {
        if matches!(arg_type(s, i), Type::None | Type::Nil) {
            Ok(None)
        } else {
            Self::check(s, i).map(Some)
        }
    }

    /// Both steps, for natives written by hand (owned types only).
    #[inline(always)]
    fn arg(s: &'a mut Stack<'_>, i: i32) -> StackResult<Self> {
        let c = Self::check(s, i)?;
        Ok(Self::get(s, i, c))
    }
}

/// Owned and `Copy` parameters: `check` does everything.
macro_rules! owned_arg {
    ($($t:ty => |$s:ident, $i:ident| $e:expr;)*) => {$(
        impl<'a> Arg<'a> for $t {
            type Checked = $t;

            #[inline(always)]
            fn check($s: &mut Stack<'_>, $i: i32) -> StackResult<$t> {
                $e
            }

            #[inline(always)]
            fn get(_: &'a Stack<'_>, _: i32, v: $t) -> $t {
                v
            }
        }
    )*};
}

owned_arg! {
    i64 => |s, i| s.check_integer(i);
    f64 => |s, i| s.check_number(i);
    // Lua's truthiness, as `lua_toboolean`: any value (or none) is a bool.
    bool => |s, i| Ok(s.to_boolean(i));
    OrbitString => |s, i| s.check_string(i).map(OrbitStr::to_owned_string);
    Vec<u8> => |s, i| s.check_string(i).map(|b| b.to_vec());
    String => |s, i| s.check_string(i).map(|b| b.to_string_lossy().into_owned());
    // Any value at all, by its index (`luaL_checkany`).
    Rest => |s, i| Ok(Rest { first: i, count: (s.top() - i + 1).max(0) as usize });
}

// Type tests read the argument's slot directly (arguments are never
// pseudo-indices), and the errors are built out of line, only when raised.

#[inline(always)]
fn arg_type(s: &Stack<'_>, i: i32) -> Type {
    s.arg_type(i)
}

#[cold]
#[inline(never)]
fn missing(s: &mut Stack<'_>, i: i32) -> Raised {
    s.arg_error(i, crate::arg_errors::VALUE_EXPECTED)
}

#[cold]
#[inline(never)]
fn not_a(s: &mut Stack<'_>, i: i32, expected: Type) -> Raised {
    s.type_error(i, expected.name())
}

/// Parameters that only need a type test and keep the argument's index.
macro_rules! typed_index_arg {
    ($($t:ident $want:ident;)*) => {$(
        impl<'a> Arg<'a> for $t {
            type Checked = $t;

            #[inline(always)]
            fn check(s: &mut Stack<'_>, i: i32) -> StackResult<$t> {
                if arg_type(s, i) == Type::$want {
                    Ok($t(i))
                } else {
                    Err(not_a(s, i, Type::$want))
                }
            }

            #[inline(always)]
            fn check_opt(s: &mut Stack<'_>, i: i32) -> StackResult<Option<$t>> {
                match arg_type(s, i) {
                    Type::$want => Ok(Some($t(i))),
                    Type::None | Type::Nil => Ok(None),
                    _ => Err(not_a(s, i, Type::$want)),
                }
            }

            #[inline(always)]
            fn get(_: &'a Stack<'_>, _: i32, v: $t) -> $t {
                v
            }
        }
    )*};
}

typed_index_arg! {
    TableArg Table;
    FunctionArg Function;
}

/// Any value at all (`luaL_checkany`).
impl<'a> Arg<'a> for Any {
    type Checked = Any;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Any> {
        if arg_type(s, i) == Type::None {
            Err(missing(s, i))
        } else {
            Ok(Any(i))
        }
    }

    #[inline(always)]
    fn get(_: &'a Stack<'_>, _: i32, v: Any) -> Any {
        v
    }
}

/// A number that may be an integer or a float, as `math.abs` takes one:
/// an integer argument stays one, and a float or a numeric string reads as
/// a float.
impl<'a> Arg<'a> for Number {
    type Checked = Number;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Number> {
        match s.number_at(i) {
            Some(n) => Ok(n),
            None => s.check_number(i).map(Number::Float),
        }
    }

    #[inline(always)]
    fn get(_: &'a Stack<'_>, _: i32, v: Number) -> Number {
        v
    }
}

/// A Rust type scripts hold as userdata (made with
/// [`Stack::push_userdata`]), which a native takes as a [`Ud`] argument.
pub trait UserType: Clone + 'static {
    /// The type's name in type errors (`FILE* expected, got nil`): the name
    /// its metatable has as `__name`.
    const NAME: &'static str;
}

/// A userdata argument holding a `T`: a copy of the value, so the native
/// can use the stack while it has it. For a value a native changes, `T` is
/// a shared handle (`Rc<RefCell<..>>`). Anything else raises Lua's type
/// error, naming [`UserType::NAME`].
#[derive(Debug, Clone)]
pub struct Ud<T>(pub T);

impl<T> std::ops::Deref for Ud<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<'a, T: UserType> Arg<'a> for Ud<T> {
    type Checked = Ud<T>;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Ud<T>> {
        // A value taken out of its userdata reads as none.
        match s.with_userdata::<T, _>(i, |v| v.clone()).flatten() {
            Some(v) => Ok(Ud(v)),
            None => Err(not_a_userdata(s, i, T::NAME)),
        }
    }

    #[inline(always)]
    fn get(_: &'a Stack<'_>, _: i32, v: Ud<T>) -> Ud<T> {
        v
    }
}

#[cold]
#[inline(never)]
fn not_a_userdata(s: &mut Stack<'_>, i: i32, name: &str) -> Raised {
    s.type_error(i, name)
}

/// The isolate's state of type `T` (see [`Stack::extension`]), made with
/// `Default` on first use. As a `#[native]` parameter it takes no argument:
/// the attribute fills it before reading the arguments. It holds a copy, so
/// state a native changes is a shared handle (`Rc<Cell<..>>`), or is changed
/// with [`Stack::extension`].
#[derive(Debug, Clone, Default)]
pub struct State<T>(pub T);

impl<T: std::any::Any + Default + Clone> State<T> {
    /// A copy of the isolate's `T`.
    #[inline(always)]
    pub fn of(s: &mut Stack<'_>) -> State<T> {
        State(s.extension(|v: &mut T| v.clone()))
    }
}

impl<T> std::ops::Deref for State<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

#[cold]
#[inline(never)]
fn not_utf8(s: &mut Stack<'_>, i: i32) -> Raised {
    s.arg_error(i, "invalid UTF-8")
}

/// A string argument (numbers convert), borrowed from the stack.
impl<'a> Arg<'a> for OrbitStr<'a> {
    type Checked = Gc<LuaStr>;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Gc<LuaStr>> {
        s.check_string_handle(i)
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, _: i32, h: Gc<LuaStr>) -> OrbitStr<'a> {
        s.handle_bytes(h).into()
    }
}

impl<'a> Arg<'a> for &'a [u8] {
    type Checked = Gc<LuaStr>;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Gc<LuaStr>> {
        s.check_string_handle(i)
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, _: i32, h: Gc<LuaStr>) -> &'a [u8] {
        s.handle_bytes(h)
    }
}

impl<'a> Arg<'a> for &'a str {
    type Checked = Gc<LuaStr>;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Gc<LuaStr>> {
        let h = s.check_string_handle(i)?;
        if std::str::from_utf8(s.handle_bytes(h)).is_ok() {
            Ok(h)
        } else {
            Err(not_utf8(s, i))
        }
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, _: i32, h: Gc<LuaStr>) -> &'a str {
        // `check` found it UTF-8.
        std::str::from_utf8(s.handle_bytes(h)).unwrap_or_default()
    }
}

/// Absent or nil reads as `None` (`luaL_opt`).
impl<'a, T: Arg<'a>> Arg<'a> for Option<T> {
    type Checked = Option<T::Checked>;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<Self::Checked> {
        T::check_opt(s, i)
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, i: i32, checked: Self::Checked) -> Option<T> {
        checked.map(|c| T::get(s, i, c))
    }
}

/// An argument of any type (but present), by its index: for values the
/// native only copies or inspects with the stack API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Any(pub i32);

/// A table argument, by its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableArg(pub i32);

/// A function argument, by its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionArg(pub i32);

/// The remaining arguments (Lua's `...`): `count` of them from index
/// `first`. Only as the last parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rest {
    pub first: i32,
    pub count: usize,
}

impl Rest {
    /// Their indices.
    pub fn indices(self) -> impl Iterator<Item = i32> {
        self.first..self.first + self.count as i32
    }
}

/// One value a native can return.
pub trait Push {
    fn push(self, s: &mut Stack<'_>);
}

impl Push for i64 {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_integer(self);
    }
}

impl Push for Number {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        match self {
            Number::Int(n) => s.push_integer(n),
            Number::Float(f) => s.push_number(f),
        }
    }
}

impl Push for f64 {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_number(self);
    }
}

impl Push for bool {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_boolean(self);
    }
}

impl Push for &[u8] {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(self);
    }
}

impl Push for &str {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(self.as_bytes());
    }
}

impl Push for Vec<u8> {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(&self);
    }
}

impl Push for String {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(self.as_bytes());
    }
}

impl Push for OrbitString {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(self.as_bytes());
    }
}

impl Push for OrbitStr<'_> {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_string_result(self.as_bytes());
    }
}

/// `None` is nil.
impl<T: Push> Push for Option<T> {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        match self {
            Some(v) => v.push(s),
            None => s.push_nil(),
        }
    }
}

/// Lua's nil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nil;

impl Push for Nil {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_nil();
    }
}

/// A thread (coroutine) argument.
impl<'a> Arg<'a> for super::ThreadRef {
    type Checked = super::ThreadRef;

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<super::ThreadRef> {
        match s.to_thread(i) {
            Some(t) if i <= s.top() => Ok(t),
            _ => Err(not_a(s, i, Type::Thread)),
        }
    }

    #[inline(always)]
    fn get(_: &'a Stack<'_>, _: i32, t: super::ThreadRef) -> super::ThreadRef {
        t
    }
}

impl Push for super::ThreadRef {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_thread(self);
    }
}

/// A string result built in the isolate's scratch buffer
/// ([`Stack::take_buffer`]), which goes back for reuse once pushed.
#[derive(Debug)]
pub(crate) struct Buffer(pub Vec<u8>);

impl Push for Buffer {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_buffer(self.0);
    }
}

/// The value at an index (an argument, an upvalue), pushed again.
impl Push for Any {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_copy(self.0);
    }
}

impl Push for TableArg {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_copy(self.0);
    }
}

/// What a native's body can return: one value, a tuple of them, nothing,
/// a [`Return`] it built by hand, or a `Result` of any of those.
pub trait Results {
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return>;
}

impl<T: Push> Results for T {
    #[inline(always)]
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        self.push(s);
        Ok(Return::values(1))
    }
}

impl Results for () {
    #[inline(always)]
    fn push_results(self, _: &mut Stack<'_>) -> StackResult<Return> {
        Ok(Return::values(0))
    }
}

/// The body pushed its results itself.
impl Results for Return {
    #[inline(always)]
    fn push_results(self, _: &mut Stack<'_>) -> StackResult<Return> {
        Ok(self)
    }
}

macro_rules! tuple_results {
    ($n:literal: $($t:ident $v:ident),+) => {
        impl<$($t: Push),+> Results for ($($t,)+) {
            #[inline(always)]
            fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
                let ($($v,)+) = self;
                $($v.push(s);)+
                Ok(Return::values($n))
            }
        }
    };
}

tuple_results!(1: A a);
tuple_results!(2: A a, B b);
tuple_results!(3: A a, B b, C c);
tuple_results!(4: A a, B b, C c, D d);

/// One set of results or another, such as `true` or `nil, message, code`
/// (`luaL_fileresult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Either<A, B> {
    Left(A),
    Right(B),
}

impl<A: Results, B: Results> Results for Either<A, B> {
    #[inline(always)]
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        match self {
            Either::Left(a) => a.push_results(s),
            Either::Right(b) => b.push_results(s),
        }
    }
}

impl<T: Results, E: IntoRaised> Results for Result<T, E> {
    #[inline(always)]
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        match self {
            Ok(v) => v.push_results(s),
            Err(e) => Err(e.into_raised(s)),
        }
    }
}

/// An error a native's body can return: raised when the body returns.
pub trait IntoRaised {
    fn into_raised(self, s: &mut Stack<'_>) -> Raised;
}

/// Already raised: passed through as is.
impl IntoRaised for Raised {
    #[inline(always)]
    fn into_raised(self, _: &mut Stack<'_>) -> Raised {
        self
    }
}

/// A runtime error with this message, positioned at the caller.
impl IntoRaised for &str {
    #[inline(always)]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        s.error(self)
    }
}

impl IntoRaised for String {
    #[inline(always)]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        s.error(&self)
    }
}

/// A message that need not be UTF-8, such as one quoting a script's string.
impl IntoRaised for OrbitString {
    #[inline(always)]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        s.error(&self)
    }
}

impl IntoRaised for Fail {
    #[inline(always)]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        self.raise(s)
    }
}

/// An error a native's body returns, raised when it returns. Bodies that
/// don't take the stack can't make a [`Raised`] themselves; these describe
/// one. `?` on a [`StackResult`] converts.
#[derive(Debug)]
pub enum Fail {
    /// Already raised.
    Raised(Raised),
    /// A runtime error with this message, positioned at the caller.
    Message(OrbitString),
    /// `bad argument #i to 'f' (message)`.
    Arg(i32, OrbitString),
    /// `bad argument #i to 'f' (T expected, got U)`.
    Type(i32, &'static str),
}

impl Fail {
    /// `bad argument #i to 'f' (message)`.
    pub fn arg(i: i32, message: impl Into<OrbitString>) -> Fail {
        Fail::Arg(i, message.into())
    }

    #[cold]
    #[inline(never)]
    fn raise(self, s: &mut Stack<'_>) -> Raised {
        match self {
            Fail::Raised(r) => r,
            Fail::Message(m) => s.error(&m),
            Fail::Arg(i, m) => s.arg_error(i, &m),
            Fail::Type(i, t) => s.type_error(i, t),
        }
    }
}

impl From<Raised> for Fail {
    fn from(r: Raised) -> Fail {
        Fail::Raised(r)
    }
}

impl From<String> for Fail {
    fn from(m: String) -> Fail {
        Fail::Message(m.into())
    }
}

impl From<&str> for Fail {
    fn from(m: &str) -> Fail {
        Fail::Message(m.into())
    }
}

impl From<OrbitString> for Fail {
    fn from(m: OrbitString) -> Fail {
        Fail::Message(m)
    }
}

/// Native `N` as a value to [`Push`]: `s.raw_put(t, b"f", function::<f>())`.
pub fn function<N: Native>() -> NativeFunction<N> {
    NativeFunction(std::marker::PhantomData)
}

/// What [`function`] gives.
#[derive(Debug)]
pub struct NativeFunction<N>(std::marker::PhantomData<N>);

impl<N: Native> Push for NativeFunction<N> {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_native::<N>();
    }
}

impl Stack<'_> {
    /// `t[k] = v` without metamethods, for the table at `idx` (which may be
    /// relative: `-1` is the table on top before `v` is pushed).
    #[inline]
    pub fn raw_put(&mut self, idx: i32, k: &[u8], v: impl Push) -> StackResult<()> {
        let t = self.abs_index(idx);
        v.push(self);
        self.raw_set_field(t, k)
    }

    /// Put native `N` in the table at `idx` under its field name
    /// ([`field_name`]): `s.raw_put_native::<date>(-1)` for a native named
    /// `os.date` sets `date`.
    #[inline]
    pub fn raw_put_native<N: Native>(&mut self, idx: i32) -> StackResult<()> {
        self.raw_put(idx, field_name(N::NAME).as_bytes(), function::<N>())
    }
}

/// The field a native named `name` is registered under: the part after
/// its last `.` (`os.date` is `date`, `FILE*.__gc` is `__gc`), or all of it.
pub fn field_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Run a native's body, raising a panic in it as a script error, as a
/// `orbit::Call` closure does. What `#[native]` wraps each function
/// in. Where panics abort, it is the body alone.
#[doc(hidden)]
#[inline(always)]
pub fn catch_panic(
    s: &mut Stack<'_>,
    body: impl FnOnce(&mut Stack<'_>) -> StackResult<Return>,
) -> StackResult<Return> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&mut *s))) {
        Ok(r) => r,
        Err(p) => Err(panicked(s, &*p)),
    }
}

#[cold]
#[inline(never)]
fn panicked(s: &mut Stack<'_>, p: &(dyn std::any::Any + Send)) -> Raised {
    s.error(panic_message(p).as_str())
}

/// The message a caught panic raises: `host function panicked: ...`, with
/// the panic's own text when it has one.
#[doc(hidden)]
pub fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    let what = p
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic");
    format!("host function panicked: {what}")
}

// ---- `#[native(pure)]` fast paths ----------------------------------------

/// A value as a `pure` native's fast path reads and returns it.
#[doc(hidden)]
pub use crate::object::Value as PureValue;

/// A `pure` native's fast path ([`Native::PURE`]): the result from the
/// arguments, or `None` to run the full native instead.
#[doc(hidden)]
pub type PureFn = fn(&[PureValue]) -> Option<PureValue>;

/// A parameter a `pure` native's fast path can read: `None` when the
/// argument (`None` if missing) doesn't already have the type, and the
/// full native then converts it or raises the error.
#[doc(hidden)]
pub trait PureArg: Sized {
    fn from_value(v: Option<PureValue>) -> Option<Self>;
}

impl PureArg for i64 {
    #[inline(always)]
    fn from_value(v: Option<PureValue>) -> Option<i64> {
        match v {
            Some(PureValue::Int(i)) => Some(i),
            _ => None,
        }
    }
}

/// An integer converts as the full native's `check_number` converts it.
impl PureArg for f64 {
    #[inline(always)]
    fn from_value(v: Option<PureValue>) -> Option<f64> {
        match v {
            Some(PureValue::Float(f)) => Some(f),
            Some(PureValue::Int(i)) => Some(i as f64),
            _ => None,
        }
    }
}

impl PureArg for Number {
    #[inline(always)]
    fn from_value(v: Option<PureValue>) -> Option<Number> {
        match v {
            Some(PureValue::Int(i)) => Some(Number::Int(i)),
            Some(PureValue::Float(f)) => Some(Number::Float(f)),
            _ => None,
        }
    }
}

/// Missing or nil is `None`, as for [`Arg`].
impl<T: PureArg> PureArg for Option<T> {
    #[inline(always)]
    fn from_value(v: Option<PureValue>) -> Option<Option<T>> {
        match v {
            None | Some(PureValue::Nil) => Some(None),
            v => T::from_value(v).map(Some),
        }
    }
}

/// What a `pure` native's body can return: one value, made without the
/// heap, or `None` to give up (an `Err`).
#[doc(hidden)]
pub trait PureResult {
    fn into_value(self) -> Option<PureValue>;
}

impl PureResult for i64 {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        Some(PureValue::Int(self))
    }
}

impl PureResult for f64 {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        Some(PureValue::Float(self))
    }
}

impl PureResult for Number {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        Some(match self {
            Number::Int(i) => PureValue::Int(i),
            Number::Float(f) => PureValue::Float(f),
        })
    }
}

impl PureResult for bool {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        Some(PureValue::Bool(self))
    }
}

impl<T: PureResult> PureResult for Option<T> {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        match self {
            Some(v) => v.into_value(),
            None => Some(PureValue::Nil),
        }
    }
}

impl<T: PureResult, E> PureResult for Result<T, E> {
    #[inline(always)]
    fn into_value(self) -> Option<PureValue> {
        self.ok()?.into_value()
    }
}
