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

use super::{Raised, Return, Stack, StackResult, Type};

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
    Vec<u8> => |s, i| s.check_string(i).map(<[u8]>::to_vec);
    String => |s, i| s.check_string(i).map(|b| String::from_utf8_lossy(b).into_owned());
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
    s.arg_error(i, "value expected")
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

#[cold]
#[inline(never)]
fn not_utf8(s: &mut Stack<'_>, i: i32) -> Raised {
    s.arg_error(i, "invalid UTF-8")
}

impl<'a> Arg<'a> for &'a [u8] {
    type Checked = ();

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<()> {
        s.check_string(i).map(drop)
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, i: i32, (): ()) -> &'a [u8] {
        // `check` left a string there.
        s.string_at(i).unwrap_or_default()
    }
}

impl<'a> Arg<'a> for &'a str {
    type Checked = ();

    #[inline(always)]
    fn check(s: &mut Stack<'_>, i: i32) -> StackResult<()> {
        let ok = std::str::from_utf8(s.check_string(i)?).is_ok();
        if ok { Ok(()) } else { Err(not_utf8(s, i)) }
    }

    #[inline(always)]
    fn get(s: &'a Stack<'_>, i: i32, (): ()) -> &'a str {
        std::str::from_utf8(s.string_at(i).unwrap_or_default()).unwrap_or_default()
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
        s.push_bytes(self);
    }
}

impl Push for &str {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_bytes(self.as_bytes());
    }
}

impl Push for Vec<u8> {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_bytes(&self);
    }
}

impl Push for String {
    #[inline(always)]
    fn push(self, s: &mut Stack<'_>) {
        s.push_bytes(self.as_bytes());
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
        s.error(self)
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
    Message(String),
    /// `bad argument #i to 'f' (message)`.
    Arg(i32, String),
    /// `bad argument #i to 'f' (T expected, got U)`.
    Type(i32, &'static str),
}

impl Fail {
    /// `bad argument #i to 'f' (message)`.
    pub fn arg(i: i32, message: impl Into<String>) -> Fail {
        Fail::Arg(i, message.into())
    }

    #[cold]
    #[inline(never)]
    fn raise(self, s: &mut Stack<'_>) -> Raised {
        match self {
            Fail::Raised(r) => r,
            Fail::Message(m) => s.error(m),
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
        Fail::Message(m)
    }
}

impl From<&str> for Fail {
    fn from(m: &str) -> Fail {
        Fail::Message(m.to_owned())
    }
}

/// Native `N` as a value to [`Push`]: `s.raw_put(t, b"f", function::<f>())`.
pub fn function<N: super::Native>() -> NativeFunction<N> {
    NativeFunction(std::marker::PhantomData)
}

/// What [`function`] gives.
#[derive(Debug)]
pub struct NativeFunction<N>(std::marker::PhantomData<N>);

impl<N: super::Native> Push for NativeFunction<N> {
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
}
