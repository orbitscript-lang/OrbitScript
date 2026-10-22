//! The `ffi` library: calling C functions in shared libraries from Lua and
//! OrbitScript without writing a C module (ADR-9, design in
//! `docs/design/ffi.md`).
//!
//! ```lua
//! local libc = ffi.open("libc.so.6")          -- ffi.open() for the process
//! local strlen = libc:bind("strlen", "size", "cstr")
//! print(strlen("hello"))                      --> 5
//! ```
//!
//! The library is built on the public stack API ([`orbit::stack`]), like
//! any host library. An FFI function is a plain native whose data words
//! ([`Control::register_native`](orbit::stack::Control::register_native))
//! hold the C function's address and which of the isolate's signatures it
//! has; libffi makes the call. The host decides whether scripts get the
//! library at all ([`install`]); once they have it they can call any C
//! function and read and write any memory, as a C module could. All
//! `unsafe` code is in [`sys`].

mod sys;

use std::cell::Cell;
use std::rc::Rc;

use orbit::OrbitString;
use orbit::stack::{
    Native, NativeRef, REGISTRY_INDEX, RIDX_GLOBALS, Return, Stack, StackResult, Type,
};

/// A C type the library converts values to and from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Ty {
    Void = 0,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Ptr,
    /// A string copied in with a terminating NUL; as a result or a field,
    /// the NUL-terminated string at the pointer, copied out.
    CStr,
    /// A string's bytes, passed as a pointer without a copy or a NUL, for
    /// C to read only during the call (arguments only).
    Bytes,
    Double,
    Float,
}

impl Ty {
    fn parse(name: &[u8]) -> Option<Ty> {
        Some(match name {
            b"void" => Ty::Void,
            b"i8" | b"char" | b"schar" => Ty::I8,
            b"u8" | b"uchar" => Ty::U8,
            b"i16" | b"short" => Ty::I16,
            b"u16" | b"ushort" => Ty::U16,
            b"i32" | b"int" => Ty::I32,
            b"u32" | b"uint" => Ty::U32,
            b"i64" | b"long" | b"llong" | b"ssize" | b"intptr" => Ty::I64,
            b"u64" | b"ulong" | b"ullong" | b"size" | b"uintptr" => Ty::U64,
            b"ptr" => Ty::Ptr,
            b"cstr" => Ty::CStr,
            b"bytes" => Ty::Bytes,
            b"double" | b"f64" => Ty::Double,
            b"float" | b"f32" => Ty::Float,
            _ => return None,
        })
    }

    /// How the value travels: strings as pointers.
    fn ctype(self) -> sys::CType {
        use sys::CType as C;
        match self {
            Ty::Void => C::Void,
            Ty::I8 => C::I8,
            Ty::U8 => C::U8,
            Ty::I16 => C::I16,
            Ty::U16 => C::U16,
            Ty::I32 => C::I32,
            Ty::U32 => C::U32,
            Ty::I64 => C::I64,
            Ty::U64 => C::U64,
            Ty::Ptr | Ty::CStr | Ty::Bytes => C::Pointer,
            Ty::Double => C::Double,
            Ty::Float => C::Float,
        }
    }

    /// Bytes in memory (`ffi.sizeof`, `ffi.get`, `ffi.set`); `None` for
    /// types that are no memory layout of their own.
    fn size(self) -> Option<usize> {
        Some(match self {
            Ty::I8 | Ty::U8 => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::I32 | Ty::U32 | Ty::Float => 4,
            Ty::I64 | Ty::U64 | Ty::Double | Ty::Ptr | Ty::CStr => 8,
            Ty::Void | Ty::Bytes => return None,
        })
    }
}

/// A C function signature and libffi's call description for it, shared by
/// every function bound with it.
#[derive(Debug)]
struct Signature {
    ret: Ty,
    args: Box<[Ty]>,
    /// Some argument is a `cstr` or `bytes`.
    has_strings: bool,
    cif: sys::Cif,
}

/// An open shared library (`ffi.open`).
#[derive(Debug)]
struct Library {
    handle: u64,
}

/// Memory a script owns, for C to fill or read (`ffi.buffer`). The cells
/// never move while the buffer lives.
#[derive(Debug)]
struct Buffer(Box<[Cell<u8>]>);

impl Buffer {
    fn addr(&self) -> u64 {
        self.0.as_ptr() as u64
    }
}

/// Per-isolate state. Natives are never freed, so there is one function
/// per distinct address and signature: binding the same function twice
/// gives the same value, and the natives stay bounded by what scripts
/// declare. Binding is rare next to calling, so lists do.
#[derive(Default)]
struct State {
    installed: bool,
    /// Every distinct signature bound; an FFI function's second data word
    /// is its index here.
    signatures: Vec<Rc<Signature>>,
    /// Functions by their data words, [address, signature index].
    functions: Vec<([u64; 2], NativeRef)>,
}

thread_local! {
    /// `errno` as the last FFI call left it.
    static ERRNO: Cell<i32> = const { Cell::new(0) };
}

const LIBRARY_META: &str = "ffi.library";
const BUFFER_META: &str = "ffi.buffer";

/// A native type for each function the library has.
macro_rules! natives {
    ($($native:ident $name:literal $f:ident;)*) => {$(
        struct $native;

        impl Native for $native {
            const NAME: &'static str = $name;

            fn call(s: &mut Stack<'_>) -> StackResult<Return> {
                $f(s)
            }
        }
    )*};
}

natives! {
    Open "ffi.open" ffi_open;
    Bind "ffi.bind" ffi_bind;
    NewBuffer "ffi.buffer" ffi_buffer;
    ToString "ffi.string" ffi_string;
    Get "ffi.get" ffi_get;
    Set "ffi.set" ffi_set;
    Offset "ffi.offset" ffi_offset;
    SizeOf "ffi.sizeof" ffi_sizeof;
    Errno "ffi.errno" ffi_errno;
    Call "ffi function" call;
    LibraryToString "ffi.library.__tostring" library_tostring;
    BufferToString "ffi.buffer.__tostring" buffer_tostring;
    BufferLen "ffi.buffer.__len" buffer_len;
}

/// Pushes a function.
type Push = for<'a, 'b> fn(&'a mut Stack<'b>);

fn push<N: Native>(s: &mut Stack<'_>) {
    s.push_native::<N>();
}

const FUNCTIONS: &[(&[u8], Push)] = &[
    (b"open", push::<Open>),
    (b"bind", push::<Bind>),
    (b"buffer", push::<NewBuffer>),
    (b"string", push::<ToString>),
    (b"get", push::<Get>),
    (b"set", push::<Set>),
    (b"offset", push::<Offset>),
    (b"sizeof", push::<SizeOf>),
    (b"errno", push::<Errno>),
];

/// Open the `ffi` library: the `ffi` global and `package.loaded.ffi`.
/// Scripts can then call any C function in any library the process can
/// load, so only hosts that trust their scripts as much as native code
/// should call this. Installing twice does nothing. Leaves the stack as it
/// was.
pub fn install(s: &mut Stack<'_>) -> StackResult<()> {
    if s.extension(|st: &mut State| std::mem::replace(&mut st.installed, true)) {
        return Ok(());
    }
    s.create_table(0, FUNCTIONS.len() as i32);
    for &(name, push) in FUNCTIONS {
        push(s);
        s.raw_set_field(-2, name)?;
    }
    // [ffi]
    new_meta(s, LIBRARY_META, push::<LibraryToString>)?;
    s.raw_get_field(-3, b"bind")?;
    s.raw_set_field(-2, b"bind")?;
    s.set_top(-3)?;
    new_meta(s, BUFFER_META, push::<BufferToString>)?;
    s.set_top(-2)?;
    s.push_native::<BufferLen>();
    s.raw_set_field(-2, b"__len")?;
    s.set_top(-2)?;
    // `package.loaded.ffi`, then the global.
    if s.raw_get_field(REGISTRY_INDEX, b"_LOADED")? != Type::Table {
        s.set_top(-2)?;
        s.create_table(0, 1);
        s.push_copy(-1);
        s.raw_set_field(REGISTRY_INDEX, b"_LOADED")?;
    }
    s.push_copy(-2);
    s.raw_set_field(-2, b"ffi")?;
    s.raw_get_index(REGISTRY_INDEX, RIDX_GLOBALS)?;
    s.push_copy(-3);
    s.raw_set_field(-2, b"ffi")?;
    s.set_top(-4)
}

/// Push a metatable for userdata named `name`, kept in the registry under
/// that name, with `__name`, `__tostring` (pushed by `tostring`) and an
/// `__index` table for methods, which is pushed after it.
fn new_meta(s: &mut Stack<'_>, name: &str, tostring: Push) -> StackResult<()> {
    s.create_table(0, 3);
    s.push_bytes(name.as_bytes());
    s.raw_set_field(-2, b"__name")?;
    tostring(s);
    s.raw_set_field(-2, b"__tostring")?;
    s.push_copy(-1);
    s.raw_set_field(REGISTRY_INDEX, name.as_bytes())?;
    // [meta]
    s.create_table(0, 1);
    s.push_copy(-1);
    s.raw_set_field(-3, b"__index")
}

/// Push a userdata holding `data`, with the metatable registered under
/// `meta`.
fn push_object<T: std::any::Any>(s: &mut Stack<'_>, data: T, meta: &str) -> StackResult<()> {
    s.push_userdata(data);
    s.raw_get_field(REGISTRY_INDEX, meta.as_bytes())?;
    s.set_metatable(-2)
}

fn type_arg(s: &mut Stack<'_>, arg: i32) -> StackResult<Ty> {
    let name = s.check_string(arg)?;
    if let Some(t) = Ty::parse(name) {
        return Ok(t);
    }
    let msg = OrbitString::concat(&[b"unknown C type '", name, b"'"]);
    Err(s.arg_error(arg, &msg))
}

/// Whether argument `arg` is nil or absent.
fn is_nil(s: &Stack<'_>, arg: i32) -> bool {
    matches!(s.type_at(arg), Type::Nil | Type::None)
}

/// `ffi.open([name])`: a shared library by file name or path, or with no
/// name the symbols already in the process.
fn ffi_open(s: &mut Stack<'_>) -> StackResult<Return> {
    let name = if is_nil(s, 1) {
        None
    } else {
        Some(s.check_string(1)?.to_vec())
    };
    match sys::open(name.as_deref()) {
        Ok(handle) => {
            push_object(s, Library { handle }, LIBRARY_META)?;
            Ok(Return::values(1))
        }
        Err(e) => {
            let name = name.as_deref().unwrap_or_default();
            let why = e.to_string();
            let msg =
                OrbitString::concat(&[b"cannot open library '", name, b"': ", why.as_bytes()]);
            Err(s.error(&msg))
        }
    }
}

/// `ffi.bind(lib, name, result, args...)` (also `lib:bind(...)`): the C
/// function `name` in `lib` as a function value.
fn ffi_bind(s: &mut Stack<'_>) -> StackResult<Return> {
    let handle = s.check_userdata(1, LIBRARY_META, |l: &mut Library| l.handle)?;
    let name = s.check_string(2)?.to_vec();
    let ret = type_arg(s, 3)?;
    if ret == Ty::Bytes {
        return Err(s.arg_error(3, "'bytes' is an argument type only"));
    }
    let nargs = (s.top() - 3).max(0);
    let mut args = Vec::with_capacity(nargs as usize);
    for arg in 4..4 + nargs {
        let t = type_arg(s, arg)?;
        if t == Ty::Void {
            return Err(s.arg_error(arg, "'void' is a result type only"));
        }
        args.push(t);
    }
    let addr = match sys::symbol(handle, &name) {
        Ok(a) => a,
        Err(e) => {
            let why = e.to_string();
            let msg =
                OrbitString::concat(&[b"cannot find C function '", &name, b"': ", why.as_bytes()]);
            return Err(s.error(&msg));
        }
    };
    let index = match s.extension(|st: &mut State| signature(st, ret, &args)) {
        Ok(i) => i,
        Err(e) => return Err(s.error(&e)),
    };
    let data = [addr, index as u64];
    let known = s.extension(|st: &mut State| {
        st.functions
            .iter()
            .find(|(d, _)| *d == data)
            .map(|&(_, f)| f)
    });
    let f = match known {
        Some(f) => f,
        None => {
            let f = s.control().register_native::<Call>(data);
            s.extension(|st: &mut State| st.functions.push((data, f)));
            f
        }
    };
    s.push_registered(f);
    Ok(Return::values(1))
}

/// The index in [`State::signatures`] of `ret` and `args`, prepared on
/// first use.
fn signature(state: &mut State, ret: Ty, args: &[Ty]) -> Result<usize, String> {
    if let Some(i) = state
        .signatures
        .iter()
        .position(|s| s.ret == ret && *s.args == *args)
    {
        return Ok(i);
    }
    let ctypes: Vec<sys::CType> = args.iter().map(|t| t.ctype()).collect();
    let cif = sys::Cif::new(ret.ctype(), &ctypes)?;
    state.signatures.push(Rc::new(Signature {
        ret,
        args: args.into(),
        has_strings: args.iter().any(|t| matches!(t, Ty::CStr | Ty::Bytes)),
        cif,
    }));
    Ok(state.signatures.len() - 1)
}

/// An integer argument for a C integer of type `t`: any value either the
/// signed or the unsigned type of that width can hold, in the low bits.
#[inline(always)]
fn int_arg(s: &mut Stack<'_>, arg: i32, t: Ty) -> StackResult<u64> {
    let n = s.check_integer(arg)?;
    let (bits, signed) = match t {
        Ty::I8 => (8, true),
        Ty::U8 => (8, false),
        Ty::I16 => (16, true),
        Ty::U16 => (16, false),
        Ty::I32 => (32, true),
        Ty::U32 => (32, false),
        _ => return Ok(n as u64),
    };
    if n < -(1i64 << (bits - 1)) || n >= 1i64 << bits {
        return Err(s.arg_error(arg, "value out of range"));
    }
    Ok(extend(n as u64, t, signed))
}

/// The low bits of `r` for an integer of type `t`, sign- or zero-extended.
fn extend(r: u64, t: Ty, signed: bool) -> u64 {
    let bits = match t {
        Ty::I8 | Ty::U8 => 8,
        Ty::I16 | Ty::U16 => 16,
        Ty::I32 | Ty::U32 => 32,
        _ => return r,
    };
    let shift = 64 - bits;
    if signed {
        (((r << shift) as i64) >> shift) as u64
    } else {
        (r << shift) >> shift
    }
}

/// A pointer argument: light userdata, a buffer, or nil for NULL.
/// Run `f` on the buffer in slot `arg`, if it holds one.
fn buffer<R>(s: &mut Stack<'_>, arg: i32, f: impl FnOnce(&Buffer) -> R) -> Option<R> {
    s.with_userdata::<Buffer, _>(arg, |b| b.as_ref().map(f))
        .flatten()
}

fn ptr_arg(s: &mut Stack<'_>, arg: i32) -> StackResult<u64> {
    match s.type_at(arg) {
        Type::Nil | Type::None => Ok(0),
        Type::LightUserdata => Ok(s.to_userdata(arg) as u64),
        Type::Userdata => match buffer(s, arg, |b| b.addr()) {
            Some(addr) => Ok(addr),
            None => Err(s.type_error(arg, "pointer")),
        },
        _ => Err(s.type_error(arg, "pointer")),
    }
}

/// Push a C value of type `t` (in `bits`, as a register or memory holds
/// it) as a Lua value.
#[inline(always)]
fn push_value(s: &mut Stack<'_>, t: Ty, bits: u64) {
    match t {
        Ty::Void => s.push_nil(),
        Ty::I8 | Ty::I16 | Ty::I32 => s.push_integer(extend(bits, t, true) as i64),
        Ty::U8 | Ty::U16 | Ty::U32 => s.push_integer(extend(bits, t, false) as i64),
        Ty::I64 | Ty::U64 => s.push_integer(bits as i64),
        Ty::Ptr => {
            if bits == 0 {
                s.push_nil();
            } else {
                s.push_light_userdata(bits as usize);
            }
        }
        Ty::CStr => match sys::cstr_bytes(bits) {
            Some(b) => {
                s.push_bytes(b);
            }
            None => s.push_nil(),
        },
        Ty::Double => s.push_number(f64::from_bits(bits)),
        Ty::Float => s.push_number(f64::from(f32::from_bits(bits as u32))),
        Ty::Bytes => unreachable!("rejected as a result"),
    }
}

/// The native behind every FFI function.
fn call(s: &mut Stack<'_>) -> StackResult<Return> {
    let [addr, index] = s.native_data();
    let sig = match s.find_extension(|st: &State| Rc::clone(&st.signatures[index as usize])) {
        Some(sig) => sig,
        None => unreachable!("bound through the library"),
    };
    let n = sig.args.len();
    let mut stack = [0u64; 8];
    let mut heap = Vec::new();
    let slots: &mut [u64] = if n <= stack.len() {
        &mut stack[..n]
    } else {
        heap.resize(n, 0);
        &mut heap
    };
    if sig.has_strings {
        return call_with_strings(s, addr, &sig, slots);
    }
    for (arg, (slot, &t)) in (1..).zip(slots.iter_mut().zip(&sig.args)) {
        *slot = arg_value(s, arg, t)?;
    }
    let r = sig.cif.call(addr, slots);
    finish(s, sig.ret, r)
}

/// [`call`] for signatures with `cstr` or `bytes` arguments.
#[inline(never)]
fn call_with_strings(
    s: &mut Stack<'_>,
    addr: u64,
    sig: &Signature,
    slots: &mut [u64],
) -> StackResult<Return> {
    // First every check and conversion, which may allocate (a number
    // passed as a string becomes one) and so move short strings; then,
    // with nothing left to allocate in the heap, the string pointers.
    for (arg, (slot, &t)) in (1..).zip(slots.iter_mut().zip(&sig.args)) {
        if let Ty::CStr | Ty::Bytes = t {
            let Some(bytes) = s.to_str(arg)? else {
                return Err(s.type_error(arg, "string"));
            };
            if t == Ty::CStr && bytes.contains(&0) {
                return Err(s.arg_error(arg, "string contains a NUL byte"));
            }
        } else {
            *slot = arg_value(s, arg, t)?;
        }
    }
    // C strings: copied with a NUL, into this stack buffer while they fit
    // and into their own allocations after that; neither moves during the
    // call.
    let mut stack = [0u8; 256];
    let mut used = 0;
    let mut long: Vec<Vec<u8>> = Vec::new();
    for (arg, (slot, &t)) in (1..).zip(slots.iter_mut().zip(&sig.args)) {
        if !matches!(t, Ty::CStr | Ty::Bytes) {
            continue;
        }
        let bytes = s.string_at(arg).expect("converted above");
        *slot = if t == Ty::Bytes {
            bytes.as_ptr() as u64
        } else if used + bytes.len() < stack.len() {
            let at = used;
            stack[at..at + bytes.len()].copy_from_slice(bytes);
            stack[at + bytes.len()] = 0;
            used += bytes.len() + 1;
            stack[at..].as_ptr() as u64
        } else {
            let mut v = Vec::with_capacity(bytes.len() + 1);
            v.extend_from_slice(bytes);
            v.push(0);
            let p = v.as_ptr() as u64;
            long.push(v);
            p
        };
    }
    let r = sig.cif.call(addr, slots);
    drop(long);
    finish(s, sig.ret, r)
}

/// Argument `arg` (not a string) as a value of type `t`, in the low bits.
#[inline(always)]
fn arg_value(s: &mut Stack<'_>, arg: i32, t: Ty) -> StackResult<u64> {
    Ok(match t {
        Ty::Ptr => ptr_arg(s, arg)?,
        Ty::Double => s.check_number(arg)?.to_bits(),
        Ty::Float => u64::from((s.check_number(arg)? as f32).to_bits()),
        Ty::Void | Ty::CStr | Ty::Bytes => unreachable!("handled by the caller"),
        _ => int_arg(s, arg, t)?,
    })
}

/// Record `errno` and return result register `r` as `ret`.
#[inline(always)]
fn finish(s: &mut Stack<'_>, ret: Ty, r: u64) -> StackResult<Return> {
    ERRNO.with(|e| e.set(sys::errno()));
    if ret == Ty::Void {
        return Ok(Return::values(0));
    }
    push_value(s, ret, r);
    Ok(Return::values(1))
}

/// `ffi.errno()`: `errno` as the last FFI call left it.
fn ffi_errno(s: &mut Stack<'_>) -> StackResult<Return> {
    let e = ERRNO.with(Cell::get);
    s.push_integer(i64::from(e));
    Ok(Return::values(1))
}

/// Most bytes `ffi.buffer` allocates.
const MAX_BUFFER: i64 = 1 << 31;

/// `ffi.buffer(size)` (zeroed) or `ffi.buffer(s)` (the bytes of `s` and a
/// NUL).
fn ffi_buffer(s: &mut Stack<'_>) -> StackResult<Return> {
    let bytes: Box<[Cell<u8>]> = if s.type_at(1) == Type::String {
        let b = s.string_at(1).expect("a string");
        b.iter().copied().chain([0]).map(Cell::new).collect()
    } else {
        let n = s.check_integer(1)?;
        if !(0..=MAX_BUFFER).contains(&n) {
            return Err(s.arg_error(1, "size out of range"));
        }
        s.reserve_memory(n as usize)?;
        (0..n).map(|_| Cell::new(0)).collect()
    };
    push_object(s, Buffer(bytes), BUFFER_META)?;
    Ok(Return::values(1))
}

/// Where argument `arg` points, and how many bytes from there a script may
/// touch: `Some` for a buffer (checked), `None` for a raw pointer.
fn place(s: &mut Stack<'_>, arg: i32) -> StackResult<(u64, Option<usize>)> {
    match s.type_at(arg) {
        Type::LightUserdata => Ok((s.to_userdata(arg) as u64, None)),
        Type::Userdata => match buffer(s, arg, |b| (b.addr(), Some(b.0.len()))) {
            Some(place) => Ok(place),
            None => Err(s.type_error(arg, "pointer")),
        },
        Type::Nil | Type::None => Err(s.arg_error(arg, "null pointer")),
        _ => Err(s.type_error(arg, "pointer")),
    }
}

/// A byte offset argument (default 0) and the address it gives, checked
/// against a buffer's length for `len` bytes.
fn at(s: &mut Stack<'_>, arg: i32, base: (u64, Option<usize>), len: usize) -> StackResult<u64> {
    let off = s.opt_integer(arg, 0)?;
    if let Some(size) = base.1
        && (off < 0
            || (off as u64)
                .checked_add(len as u64)
                .is_none_or(|e| e > size as u64))
    {
        return Err(s.arg_error(arg, "out of the buffer's bounds"));
    }
    Ok(base.0.wrapping_add(off as u64))
}

/// `ffi.string(p [, len])`: `len` bytes at `p`, or up to the first NUL.
fn ffi_string(s: &mut Stack<'_>) -> StackResult<Return> {
    let base = place(s, 1)?;
    let bytes = match (is_nil(s, 2), base.1) {
        (true, None) => sys::cstr_bytes(base.0).unwrap_or_default().to_vec(),
        (true, Some(_)) => buffer(s, 1, |b| {
            b.0.iter().map(Cell::get).take_while(|&c| c != 0).collect()
        })
        .expect("a buffer"),
        (false, _) => {
            let n = s.check_integer(2)?;
            if n < 0 {
                return Err(s.arg_error(2, "negative length"));
            }
            s.reserve_memory(n as usize)?;
            let p = at(s, 3, base, n as usize)?;
            let mut out = vec![0; n as usize];
            sys::read(p, &mut out);
            out
        }
    };
    s.push_bytes(&bytes);
    Ok(Return::values(1))
}

/// The type argument of `ffi.get`/`ffi.set`/`ffi.sizeof` and its size.
fn memory_type(s: &mut Stack<'_>, arg: i32) -> StackResult<(Ty, usize)> {
    let t = type_arg(s, arg)?;
    match t.size() {
        Some(n) => Ok((t, n)),
        None => Err(s.arg_error(arg, "type has no size")),
    }
}

/// `ffi.get(p, type [, offset])`: the value of `type` at `p + offset`
/// (a `cstr` field is read as a pointer, then the string at it).
fn ffi_get(s: &mut Stack<'_>) -> StackResult<Return> {
    let base = place(s, 1)?;
    let (t, size) = memory_type(s, 2)?;
    let p = at(s, 3, base, size)?;
    let mut raw = [0u8; 8];
    sys::read(p, &mut raw[..size]);
    push_value(s, t, u64::from_le_bytes(raw));
    Ok(Return::values(1))
}

/// `ffi.set(p, type, offset, value)`: store `value` as a `type` at
/// `p + offset`.
fn ffi_set(s: &mut Stack<'_>) -> StackResult<Return> {
    let base = place(s, 1)?;
    let (t, size) = memory_type(s, 2)?;
    let bits = match t {
        Ty::CStr => return Err(s.arg_error(2, "'cstr' cannot be stored; store a 'ptr'")),
        Ty::Ptr => ptr_arg(s, 4)?,
        Ty::Double => s.check_number(4)?.to_bits(),
        Ty::Float => u64::from((s.check_number(4)? as f32).to_bits()),
        _ => int_arg(s, 4, t)?,
    };
    let p = at(s, 3, base, size)?;
    sys::write(p, &bits.to_le_bytes()[..size]);
    Ok(Return::values(0))
}

/// `ffi.offset(p, n)`: the pointer `n` bytes past `p`. For a buffer, `n`
/// must be within it; the result is a raw pointer, valid while the buffer
/// lives.
fn ffi_offset(s: &mut Stack<'_>) -> StackResult<Return> {
    let base = place(s, 1)?;
    let p = at(s, 2, base, 0)?;
    s.push_light_userdata(p as usize);
    Ok(Return::values(1))
}

/// `ffi.sizeof(type)`: its size in bytes.
fn ffi_sizeof(s: &mut Stack<'_>) -> StackResult<Return> {
    let (_, n) = memory_type(s, 1)?;
    s.push_integer(n as i64);
    Ok(Return::values(1))
}

fn library_tostring(s: &mut Stack<'_>) -> StackResult<Return> {
    let h = s.check_userdata(1, LIBRARY_META, |l: &mut Library| l.handle)?;
    s.push_bytes(format!("ffi.library: 0x{h:014x}").as_bytes());
    Ok(Return::values(1))
}

fn buffer_tostring(s: &mut Stack<'_>) -> StackResult<Return> {
    let text = s.check_userdata(1, BUFFER_META, |b: &mut Buffer| {
        format!("ffi.buffer ({} bytes): 0x{:014x}", b.0.len(), b.addr())
    })?;
    s.push_bytes(text.as_bytes());
    Ok(Return::values(1))
}

fn buffer_len(s: &mut Stack<'_>) -> StackResult<Return> {
    let n = s.check_userdata(1, BUFFER_META, |b: &mut Buffer| b.0.len())?;
    s.push_integer(n as i64);
    Ok(Return::values(1))
}
