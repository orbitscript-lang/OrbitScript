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
//! An FFI function is a plain native whose data words
//! ([`orbit_vm::Vm::register_native_with_data`]) hold the C function's
//! address and which of the VM's signatures it has; libffi makes the call. The host decides whether scripts get the library at all
//! ([`install`]); once they have it they can call any C function and read
//! and write any memory, as a C module could. All `unsafe` code is in
//! [`sys`].

mod sys;

use std::cell::Cell;
use std::rc::Rc;

use orbit_vm::orbit_core::{Gc, LightUserData, Table, UserData, Value};
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

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

/// Per-VM state. Natives are never freed, so there is one function value
/// per distinct address and signature: binding the same function twice
/// gives the same value, and the natives stay bounded by what scripts
/// declare. Binding is rare next to calling, so lists do.
#[derive(Default)]
struct State {
    installed: bool,
    /// Every distinct signature bound; an FFI function's second data word
    /// is its index here.
    signatures: Vec<Rc<Signature>>,
    /// Function values by their data words, [address, signature index].
    functions: Vec<([u64; 2], Value)>,
}

thread_local! {
    /// `errno` as the last FFI call left it.
    static ERRNO: Cell<i32> = const { Cell::new(0) };
}

const LIBRARY_META: &str = "ffi.library";
const BUFFER_META: &str = "ffi.buffer";

const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("ffi.open", ffi_open),
    ("ffi.bind", ffi_bind),
    ("ffi.buffer", ffi_buffer),
    ("ffi.string", ffi_string),
    ("ffi.get", ffi_get),
    ("ffi.set", ffi_set),
    ("ffi.offset", ffi_offset),
    ("ffi.sizeof", ffi_sizeof),
    ("ffi.errno", ffi_errno),
];

/// Open the `ffi` library on `vm`: the `ffi` global and
/// `package.loaded.ffi`. Scripts can then call any C function in any
/// library the process can load, so only hosts that trust their scripts as
/// much as native code should call this. Installing twice does nothing.
pub fn install(vm: &mut Vm) {
    let state = vm.rt.ext.get_or_insert_with(State::default);
    if std::mem::replace(&mut state.installed, true) {
        return;
    }
    let ffi = orbit_stdlib::open_lib(vm, "ffi", FUNCTIONS);
    vm.pin(Value::Table(ffi));
    let k = vm.new_str(b"bind");
    let bind = orbit_vm::orbit_core::table::get(&vm.heap, ffi, k);
    let (_, methods) = new_meta(vm, LIBRARY_META, "ffi.library.__tostring", library_tostring);
    set(vm, methods, "bind", bind);
    let (meta, _) = new_meta(vm, BUFFER_META, "ffi.buffer.__tostring", buffer_tostring);
    let len = vm.register_native("ffi.buffer.__len", buffer_len);
    set(vm, meta, "__len", len);
    vm.unpin(Value::Table(ffi));
}

fn set(vm: &mut Vm, t: Gc<Table>, k: &str, v: Value) {
    let k = vm.new_str(k.as_bytes());
    vm.table_set(t, k, v);
}

/// A metatable for userdata named `name`, kept in the registry under that
/// name, with `__name`, `__tostring` and an `__index` table for methods:
/// both tables. The registry keeps them alive.
fn new_meta(
    vm: &mut Vm,
    name: &str,
    tostring_name: &'static str,
    tostring: NativeFn,
) -> (Gc<Table>, Gc<Table>) {
    let meta = vm.new_table();
    vm.pin(Value::Table(meta));
    let index = vm.new_table();
    set(vm, meta, "__index", Value::Table(index));
    let n = vm.new_str(name.as_bytes());
    set(vm, meta, "__name", n);
    let v = vm.register_native(tostring_name, tostring);
    set(vm, meta, "__tostring", v);
    let reg = vm.rt.registry;
    set(vm, reg, name, Value::Table(meta));
    vm.unpin(Value::Table(meta));
    (meta, index)
}

fn light(bits: u64) -> Value {
    Value::LightUserData(LightUserData::new(bits))
}

/// A userdata of `T` with the metatable registered under `meta`.
fn new_object<T: std::any::Any>(ctx: &mut NativeCtx<'_>, data: T, meta: &str) -> Value {
    let k = ctx.new_str(meta.as_bytes());
    let reg = ctx.vm.rt.registry;
    let meta = match orbit_vm::orbit_core::table::get(&ctx.vm.heap, reg, k) {
        Value::Table(t) => Some(t),
        _ => None,
    };
    let u = ctx.new_userdata(data);
    ctx.vm.heap.update(u, |u: &mut UserData| u.meta = meta);
    Value::UserData(u)
}

fn type_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Ty, LuaError> {
    let s = ctx.check_str(i)?;
    match Ty::parse(ctx.bytes(s)) {
        Some(t) => Ok(t),
        None => {
            let name = String::from_utf8_lossy(ctx.bytes(s)).into_owned();
            Err(ctx.arg_error(i, format!("unknown C type '{name}'")))
        }
    }
}

/// `ffi.open([name])`: a shared library by file name or path, or with no
/// name the symbols already in the process.
fn ffi_open(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let name = match ctx.arg(0) {
        Value::Nil => None,
        _ => {
            let s = ctx.check_str(0)?;
            Some(ctx.bytes(s).to_vec())
        }
    };
    match sys::open(name.as_deref()) {
        Ok(handle) => {
            let v = new_object(ctx, Library { handle }, LIBRARY_META);
            ctx.ret1(v)
        }
        Err(e) => {
            let name = String::from_utf8_lossy(name.as_deref().unwrap_or_default()).into_owned();
            Err(ctx.error(format!("cannot open library '{name}': {e}")))
        }
    }
}

/// `ffi.bind(lib, name, result, args...)` (also `lib:bind(...)`): the C
/// function `name` in `lib` as a function value.
fn ffi_bind(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let lib = ctx.check_userdata::<Library>(0, LIBRARY_META)?;
    let handle = ctx.userdata_ref::<Library>(lib).expect("checked").handle;
    let name = ctx.check_str(1)?;
    let ret = type_arg(ctx, 2)?;
    if ret == Ty::Bytes {
        return Err(ctx.arg_error(2, "'bytes' is an argument type only"));
    }
    let nargs = ctx.nargs().saturating_sub(3);
    let mut args = Vec::with_capacity(nargs);
    for k in 0..nargs {
        let t = type_arg(ctx, 3 + k)?;
        if t == Ty::Void {
            return Err(ctx.arg_error(3 + k, "'void' is a result type only"));
        }
        args.push(t);
    }
    let name = ctx.bytes(name).to_vec();
    let addr = match sys::symbol(handle, &name) {
        Ok(a) => a,
        Err(e) => {
            let name = String::from_utf8_lossy(&name).into_owned();
            return Err(ctx.error(format!("cannot find C function '{name}': {e}")));
        }
    };
    let index = match signature(ctx.vm, ret, &args) {
        Ok(i) => i,
        Err(e) => return Err(ctx.error(e)),
    };
    let data = [addr, index as u64];
    let state = ctx.vm.rt.ext.get_or_insert_with(State::default);
    let v = match state.functions.iter().find(|(d, _)| *d == data) {
        Some(&(_, v)) => v,
        None => {
            let v = ctx.vm.register_native_with_data("ffi function", call, data);
            let state = ctx.vm.rt.ext.get_or_insert_with(State::default);
            state.functions.push((data, v));
            v
        }
    };
    ctx.ret1(v)
}

/// The index in [`State::signatures`] of `ret` and `args`, prepared on
/// first use.
fn signature(vm: &mut Vm, ret: Ty, args: &[Ty]) -> Result<usize, String> {
    let state = vm.rt.ext.get_or_insert_with(State::default);
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
#[inline]
fn int_arg(ctx: &mut NativeCtx<'_>, k: usize, t: Ty) -> Result<u64, LuaError> {
    let n = match ctx.arg(k) {
        Value::Int(n) => n,
        _ => ctx.check_int(k)?,
    };
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
        return Err(ctx.arg_error(k, "value out of range"));
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
fn ptr_arg(ctx: &mut NativeCtx<'_>, k: usize) -> Result<u64, LuaError> {
    match ctx.arg(k) {
        Value::Nil => Ok(0),
        Value::LightUserData(p) => Ok(p.get()),
        Value::UserData(u) => match ctx.userdata_ref::<Buffer>(u) {
            Some(b) => Ok(b.addr()),
            None => Err(ctx.type_error(k, "pointer")),
        },
        _ => Err(ctx.type_error(k, "pointer")),
    }
}

/// A C value of type `t` (in `bits`, as a register or memory holds it) as
/// a Lua value.
fn to_value(ctx: &mut NativeCtx<'_>, t: Ty, bits: u64) -> Value {
    match t {
        Ty::Void => Value::Nil,
        Ty::I8 | Ty::I16 | Ty::I32 => Value::Int(extend(bits, t, true) as i64),
        Ty::U8 | Ty::U16 | Ty::U32 => Value::Int(extend(bits, t, false) as i64),
        Ty::I64 | Ty::U64 => Value::Int(bits as i64),
        Ty::Ptr => {
            if bits == 0 {
                Value::Nil
            } else {
                light(bits)
            }
        }
        Ty::CStr => match sys::cstr_bytes(bits) {
            Some(b) => ctx.new_str(b),
            None => Value::Nil,
        },
        Ty::Double => Value::Float(f64::from_bits(bits)),
        Ty::Float => Value::Float(f64::from(f32::from_bits(bits as u32))),
        Ty::Bytes => unreachable!("rejected as a result"),
    }
}

/// The native behind every FFI function.
fn call(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let [addr, index] = ctx.native_data();
    let sig = match ctx.vm.rt.ext.get::<State>() {
        Some(state) => Rc::clone(&state.signatures[index as usize]),
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
        return call_with_strings(ctx, addr, &sig, slots);
    }
    for (k, (slot, &t)) in slots.iter_mut().zip(&sig.args).enumerate() {
        *slot = arg(ctx, k, t)?;
    }
    let r = sig.cif.call(addr, slots);
    finish(ctx, sig.ret, r)
}

/// [`call`] for signatures with `cstr` or `bytes` arguments.
#[inline(never)]
fn call_with_strings(
    ctx: &mut NativeCtx<'_>,
    addr: u64,
    sig: &Signature,
    slots: &mut [u64],
) -> Result<NativeReturn, LuaError> {
    // First every check and conversion, which may allocate (a number
    // passed as a string becomes one) and so move short strings; then,
    // with nothing left to allocate in the heap, the string pointers.
    let mut strs = Vec::new();
    for (k, &t) in sig.args.iter().enumerate() {
        if let Ty::CStr | Ty::Bytes = t {
            let s = ctx.check_str(k)?;
            if t == Ty::CStr && ctx.bytes(s).contains(&0) {
                return Err(ctx.arg_error(k, "string contains a NUL byte"));
            }
            strs.push((k, t, s));
        } else {
            slots[k] = arg(ctx, k, t)?;
        }
    }
    // C strings: copied with a NUL, into this stack buffer while they fit
    // and into their own allocations after that; neither moves during the
    // call.
    let mut stack = [0u8; 256];
    let mut used = 0;
    let mut long: Vec<Vec<u8>> = Vec::new();
    for &(k, t, s) in &strs {
        let bytes = ctx.bytes(s);
        slots[k] = if t == Ty::Bytes {
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
    finish(ctx, sig.ret, r)
}

/// Argument `k` (not a string) as a value of type `t`, in the low bits.
#[inline(always)]
fn arg(ctx: &mut NativeCtx<'_>, k: usize, t: Ty) -> Result<u64, LuaError> {
    Ok(match t {
        Ty::Ptr => ptr_arg(ctx, k)?,
        Ty::Double => match ctx.arg(k) {
            Value::Float(x) => x,
            _ => ctx.check_f64(k)?,
        }
        .to_bits(),
        Ty::Float => u64::from((ctx.check_f64(k)? as f32).to_bits()),
        Ty::Void | Ty::CStr | Ty::Bytes => unreachable!("handled by the caller"),
        _ => int_arg(ctx, k, t)?,
    })
}

/// Record `errno` and return result register `r` as `ret`.
#[inline(always)]
fn finish(ctx: &mut NativeCtx<'_>, ret: Ty, r: u64) -> Result<NativeReturn, LuaError> {
    ERRNO.with(|e| e.set(sys::errno()));
    if ret == Ty::Void {
        return Ok(NativeReturn::Return(0));
    }
    let v = to_value(ctx, ret, r);
    ctx.ret1(v)
}

/// `ffi.errno()`: `errno` as the last FFI call left it.
fn ffi_errno(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let e = ERRNO.with(Cell::get);
    ctx.ret1(Value::Int(i64::from(e)))
}

/// Most bytes `ffi.buffer` allocates.
const MAX_BUFFER: i64 = 1 << 31;

/// `ffi.buffer(size)` (zeroed) or `ffi.buffer(s)` (the bytes of `s` and a
/// NUL).
fn ffi_buffer(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let bytes: Box<[Cell<u8>]> = match ctx.arg(0) {
        Value::Str(s) => {
            let b = ctx.bytes(s);
            b.iter().copied().chain([0]).map(Cell::new).collect()
        }
        _ => {
            let n = ctx.check_int(0)?;
            if !(0..=MAX_BUFFER).contains(&n) {
                return Err(ctx.arg_error(0, "size out of range"));
            }
            ctx.reserve(n as usize)?;
            (0..n).map(|_| Cell::new(0)).collect()
        }
    };
    let v = new_object(ctx, Buffer(bytes), BUFFER_META);
    ctx.ret1(v)
}

/// Where argument `i` points, and how many bytes from there a script may
/// touch: `Some` for a buffer (checked), `None` for a raw pointer.
fn place(ctx: &mut NativeCtx<'_>, i: usize) -> Result<(u64, Option<usize>), LuaError> {
    match ctx.arg(i) {
        Value::LightUserData(p) => Ok((p.get(), None)),
        Value::UserData(u) => match ctx.userdata_ref::<Buffer>(u) {
            Some(b) => Ok((b.addr(), Some(b.0.len()))),
            None => Err(ctx.type_error(i, "pointer")),
        },
        Value::Nil => Err(ctx.arg_error(i, "null pointer")),
        _ => Err(ctx.type_error(i, "pointer")),
    }
}

/// A byte offset argument (default 0) and the address it gives, checked
/// against a buffer's length for `len` bytes.
fn at(
    ctx: &mut NativeCtx<'_>,
    i: usize,
    base: (u64, Option<usize>),
    len: usize,
) -> Result<u64, LuaError> {
    let off = ctx.opt_int(i, 0)?;
    if let Some(size) = base.1
        && (off < 0
            || (off as u64)
                .checked_add(len as u64)
                .is_none_or(|e| e > size as u64))
    {
        return Err(ctx.arg_error(i, "out of the buffer's bounds"));
    }
    Ok(base.0.wrapping_add(off as u64))
}

/// `ffi.string(p [, len])`: `len` bytes at `p`, or up to the first NUL.
fn ffi_string(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let base = place(ctx, 0)?;
    let bytes = match (ctx.arg(1), base.1) {
        (Value::Nil, None) => sys::cstr_bytes(base.0).unwrap_or_default().to_vec(),
        (Value::Nil, Some(_)) => {
            let Value::UserData(u) = ctx.arg(0) else {
                unreachable!("a buffer")
            };
            let b = ctx.userdata_ref::<Buffer>(u).expect("a buffer");
            b.0.iter().map(Cell::get).take_while(|&c| c != 0).collect()
        }
        _ => {
            let n = ctx.check_int(1)?;
            if n < 0 {
                return Err(ctx.arg_error(1, "negative length"));
            }
            ctx.reserve(n as usize)?;
            let p = at(ctx, 2, base, n as usize)?;
            let mut out = vec![0; n as usize];
            sys::read(p, &mut out);
            out
        }
    };
    let v = ctx.new_str(&bytes);
    ctx.ret1(v)
}

/// The type argument of `ffi.get`/`ffi.set`/`ffi.sizeof` and its size.
fn memory_type(ctx: &mut NativeCtx<'_>, i: usize) -> Result<(Ty, usize), LuaError> {
    let t = type_arg(ctx, i)?;
    match t.size() {
        Some(n) => Ok((t, n)),
        None => Err(ctx.arg_error(i, "type has no size")),
    }
}

/// `ffi.get(p, type [, offset])`: the value of `type` at `p + offset`
/// (a `cstr` field is read as a pointer, then the string at it).
fn ffi_get(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let base = place(ctx, 0)?;
    let (t, size) = memory_type(ctx, 1)?;
    let p = at(ctx, 2, base, size)?;
    let mut raw = [0u8; 8];
    sys::read(p, &mut raw[..size]);
    let v = to_value(ctx, t, u64::from_le_bytes(raw));
    ctx.ret1(v)
}

/// `ffi.set(p, type, offset, value)`: store `value` as a `type` at
/// `p + offset`.
fn ffi_set(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let base = place(ctx, 0)?;
    let (t, size) = memory_type(ctx, 1)?;
    let bits = match t {
        Ty::CStr => return Err(ctx.arg_error(1, "'cstr' cannot be stored; store a 'ptr'")),
        Ty::Ptr => ptr_arg(ctx, 3)?,
        Ty::Double => ctx.check_f64(3)?.to_bits(),
        Ty::Float => u64::from((ctx.check_f64(3)? as f32).to_bits()),
        _ => int_arg(ctx, 3, t)?,
    };
    let p = at(ctx, 2, base, size)?;
    sys::write(p, &bits.to_le_bytes()[..size]);
    Ok(NativeReturn::Return(0))
}

/// `ffi.offset(p, n)`: the pointer `n` bytes past `p`. For a buffer, `n`
/// must be within it; the result is a raw pointer, valid while the buffer
/// lives.
fn ffi_offset(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let base = place(ctx, 0)?;
    let p = at(ctx, 1, base, 0)?;
    ctx.ret1(light(p))
}

/// `ffi.sizeof(type)`: its size in bytes.
fn ffi_sizeof(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let (_, n) = memory_type(ctx, 0)?;
    ctx.ret1(Value::Int(n as i64))
}

fn library_tostring(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let u = ctx.check_userdata::<Library>(0, LIBRARY_META)?;
    let h = ctx.userdata_ref::<Library>(u).expect("checked").handle;
    let v = ctx.new_str(format!("ffi.library: 0x{h:014x}").as_bytes());
    ctx.ret1(v)
}

fn buffer_tostring(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let u = ctx.check_userdata::<Buffer>(0, BUFFER_META)?;
    let b = ctx.userdata_ref::<Buffer>(u).expect("checked");
    let text = format!("ffi.buffer ({} bytes): 0x{:014x}", b.0.len(), b.addr());
    let v = ctx.new_str(text.as_bytes());
    ctx.ret1(v)
}

fn buffer_len(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let u = ctx.check_userdata::<Buffer>(0, BUFFER_META)?;
    let n = ctx.userdata_ref::<Buffer>(u).expect("checked").0.len();
    ctx.ret1(Value::Int(n as i64))
}
