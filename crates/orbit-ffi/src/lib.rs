//! SPIKE (never merges): call C functions in dynamic libraries from Lua and
//! OrbitScript, with our own register trampolines instead of libffi. See
//! `planning/ffi/` in the project files.
//!
//! ```lua
//! local libc = ffi.open("libc.so.6")       -- or ffi.open() for the process
//! local strlen = ffi.fn(libc, "strlen", "size", "cstr")
//! print(strlen("hello"))                   --> 5
//! ```
//!
//! Types: `void` (results only), `int`/`uint` (C `int`), `long`/`ulong`
//! (64-bit), `size`, `ptr`, `cstr`, `double`, `float`. Up to six
//! integer-class and eight float arguments, no variadics, no structs by
//! value, no callbacks. A function value is a native closure over
//! [`call_ffi`] whose hidden upvalues hold the address and the packed
//! signature. All `unsafe` is in [`sys`].

mod sys;

use std::cell::Cell;

use orbit_vm::orbit_core::{LightUserData, Value};
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Ty {
    Void = 0,
    Int,
    UInt,
    Long,
    ULong,
    Ptr,
    CStr,
    Double,
    Float,
}

impl Ty {
    fn parse(s: &[u8]) -> Option<Ty> {
        Some(match s {
            b"void" => Ty::Void,
            b"int" | b"i32" => Ty::Int,
            b"uint" | b"u32" => Ty::UInt,
            b"long" | b"i64" => Ty::Long,
            b"ulong" | b"u64" | b"size" => Ty::ULong,
            b"ptr" | b"handle" => Ty::Ptr,
            b"cstr" => Ty::CStr,
            b"double" | b"f64" => Ty::Double,
            b"float" | b"f32" => Ty::Float,
            _ => return None,
        })
    }

    fn from_bits(b: u64) -> Ty {
        match b & 15 {
            0 => Ty::Void,
            1 => Ty::Int,
            2 => Ty::UInt,
            3 => Ty::Long,
            4 => Ty::ULong,
            5 => Ty::Ptr,
            6 => Ty::CStr,
            7 => Ty::Double,
            _ => Ty::Float,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Ty::Double | Ty::Float)
    }
}

/// A packed signature: bits 0-3 the result type, 4-7 the argument count,
/// then four bits per argument; bit 60 set when the fast trampoline fits.
#[derive(Clone, Copy)]
struct Sig(u64);

const FAST: u64 = 1 << 60;
/// Set when every argument (at most three) and the result are `double`.
const FAST_FLOAT: u64 = 1 << 61;
const MAX_ARGS: usize = 12;

impl Sig {
    fn ret(self) -> Ty {
        Ty::from_bits(self.0)
    }
    fn nargs(self) -> usize {
        ((self.0 >> 4) & 15) as usize
    }
    fn arg(self, k: usize) -> Ty {
        Ty::from_bits(self.0 >> (8 + 4 * k))
    }
    fn fast(self) -> bool {
        self.0 & FAST != 0
    }
}

/// Memory a script allocated for C to fill or read (`ffi.buffer`). The
/// cells never move while the userdata lives.
#[derive(Debug)]
struct Buffer(Box<[Cell<u8>]>);

const FFI_FUNCTION: &str = "ffi function";
const FFI_FUNCTION_CLOSURE: &str = "ffi function (closure)";

pub const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("ffi.open", ffi_open),
    ("ffi.fn", ffi_fn),
    ("ffi.fn_closure", ffi_fn_closure),
    ("ffi.fn_generic", ffi_fn_generic),
    ("ffi.errno", ffi_errno),
    ("ffi.buffer", ffi_buffer),
    ("ffi.string", ffi_string),
    ("ffi.native_labs", native_labs),
    ("ffi.native_strlen", native_strlen),
    ("ffi.closure_labs", closure_labs),
];

/// Install the `ffi` table (spike: always, when the feature is on).
pub fn install(vm: &mut Vm) {
    let t = vm.new_table();
    vm.pin(Value::Table(t));
    for &(qualified, f) in FUNCTIONS {
        let short = qualified.rsplit('.').next().unwrap_or(qualified);
        let fv = vm.register_native(qualified, f);
        let k = vm.new_str(short.as_bytes());
        vm.table_set(t, k, fv);
    }
    vm.set_global("ffi", Value::Table(t));
    vm.unpin(Value::Table(t));
}

fn light(bits: u64) -> Value {
    Value::LightUserData(LightUserData::new(bits))
}

fn ffi_open(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let name = match ctx.arg(0) {
        Value::Nil => None,
        _ => {
            let s = ctx.check_str(0)?;
            Some(ctx.bytes(s).to_vec())
        }
    };
    match sys::open(name.as_deref()) {
        Ok(h) => ctx.ret1(light(h)),
        Err(e) => Err(ctx.error(e)),
    }
}

fn parse_sig(ctx: &mut NativeCtx<'_>) -> Result<Sig, LuaError> {
    let ty = |ctx: &mut NativeCtx<'_>, i: usize| -> Result<Ty, LuaError> {
        let s = ctx.check_str(i)?;
        match Ty::parse(ctx.bytes(s)) {
            Some(t) => Ok(t),
            None => Err(ctx.arg_error(i, "unknown C type")),
        }
    };
    let ret = ty(ctx, 2)?;
    let nargs = ctx.nargs().saturating_sub(3);
    if nargs > MAX_ARGS {
        return Err(ctx.error("too many arguments for an ffi function"));
    }
    let mut bits = ret as u64 | (nargs as u64) << 4;
    let (mut ni, mut nf) = (0, 0);
    for k in 0..nargs {
        let t = ty(ctx, 3 + k)?;
        if t == Ty::Void {
            return Err(ctx.arg_error(3 + k, "void is not an argument type"));
        }
        if t.is_float() {
            nf += 1
        } else {
            ni += 1
        }
        bits |= (t as u64) << (8 + 4 * k);
    }
    if ni > sys::INT_REGS || nf > sys::FLOAT_REGS {
        return Err(ctx.error("arguments don't fit in registers (stack arguments unsupported)"));
    }
    if nf == 0 && ni <= 3 && !ret.is_float() {
        bits |= FAST;
    }
    if ni == 0
        && nf <= 3
        && ret == Ty::Double
        && (0..nargs).all(|k| (bits >> (8 + 4 * k)) & 15 == Ty::Double as u64)
    {
        bits |= FAST_FLOAT;
    }
    Ok(Sig(bits))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A plain native value carrying the address and signature (VM spike
    /// addition): the direct native call path.
    Direct,
    /// A native closure with the address and signature in hidden upvalues,
    /// as C module functions are made today.
    Closure,
    /// [`Mode::Closure`] without the fast trampoline.
    Generic,
}

fn make_fn(ctx: &mut NativeCtx<'_>, mode: Mode) -> Result<NativeReturn, LuaError> {
    let Value::LightUserData(lib) = ctx.arg(0) else {
        return Err(ctx.type_error(0, "library"));
    };
    let name = ctx.check_str(1)?;
    let name = ctx.bytes(name).to_vec();
    let mut sig = parse_sig(ctx)?;
    let f = sys::symbol(lib.get(), &name).map_err(|e| ctx.error(e))?;
    if mode == Mode::Generic {
        sig.0 &= !(FAST | FAST_FLOAT);
    }
    let v = if mode == Mode::Direct {
        ctx.native_with_data(FFI_FUNCTION, call_ffi_direct, [f, sig.0])
    } else {
        let hidden = [light(f), Value::Int(sig.0 as i64)];
        ctx.new_closure_hidden(FFI_FUNCTION_CLOSURE, call_ffi_closure, &[], &hidden)
    };
    ctx.ret1(v)
}

fn ffi_fn(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    make_fn(ctx, Mode::Direct)
}

fn ffi_fn_closure(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    make_fn(ctx, Mode::Closure)
}

fn ffi_fn_generic(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    make_fn(ctx, Mode::Generic)
}

fn call_ffi_direct(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let [f, sig] = ctx.native_data();
    call_ffi(ctx, f, Sig(sig))
}

fn call_ffi_closure(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let Value::LightUserData(f) = ctx.hidden_upvalue(0) else {
        unreachable!("an ffi function's first hidden upvalue is its address")
    };
    let Value::Int(sig) = ctx.hidden_upvalue(1) else {
        unreachable!("an ffi function's second hidden upvalue is its signature")
    };
    call_ffi(ctx, f.get(), Sig(sig as u64))
}

/// A pointer argument: light userdata, a buffer, or nil (NULL).
fn ptr_arg(ctx: &mut NativeCtx<'_>, i: usize) -> Result<u64, LuaError> {
    match ctx.arg(i) {
        Value::Nil => Ok(0),
        Value::LightUserData(p) => Ok(p.get()),
        Value::UserData(u) => match ctx.userdata_ref::<Buffer>(u) {
            Some(b) => Ok(b.0.as_ptr() as u64),
            None => Err(ctx.type_error(i, "pointer")),
        },
        _ => Err(ctx.type_error(i, "pointer")),
    }
}

/// Call C function `f` with signature `sig` on the native's arguments.
#[inline(always)]
fn call_ffi(ctx: &mut NativeCtx<'_>, f: u64, sig: Sig) -> Result<NativeReturn, LuaError> {
    if sig.fast()
        && let Some(r) = fast_args(ctx, sig)
    {
        let r = sys::call_int3(f, r[0], r[1], r[2]);
        return int_result(ctx, sig.ret(), r);
    }
    let mut ints = [0u64; sys::INT_REGS];
    let mut floats = [0f64; sys::FLOAT_REGS];
    let (mut ni, mut nf) = (0, 0);
    if sig.0 & FAST_FLOAT != 0 {
        let mut a = [0f64; 3];
        let mut ok = true;
        for (k, slot) in a.iter_mut().enumerate().take(sig.nargs()) {
            match ctx.arg(k) {
                Value::Float(x) => *slot = x,
                Value::Int(n) => *slot = n as f64,
                _ => ok = false,
            }
        }
        if ok {
            let r = sys::call_f3(f, a[0], a[1], a[2]);
            return ctx.ret1(Value::Float(r));
        }
    }
    // C strings: copied, NUL-terminated, into a stack buffer while they
    // fit, else into their own heap copies (neither moves during the call).
    let mut stack = [0u8; 256];
    let mut used = 0;
    let mut long: Vec<Vec<u8>> = Vec::new();
    for k in 0..sig.nargs() {
        match sig.arg(k) {
            Ty::Int | Ty::UInt | Ty::Long | Ty::ULong => {
                ints[ni] = ctx.check_int(k)? as u64;
                ni += 1;
            }
            Ty::Ptr => {
                ints[ni] = ptr_arg(ctx, k)?;
                ni += 1;
            }
            Ty::CStr => {
                let s = ctx.check_str(k)?;
                let bytes = ctx.bytes(s);
                if bytes.contains(&0) {
                    return Err(ctx.arg_error(k, "string contains a NUL byte"));
                }
                let n = bytes.len();
                ints[ni] = if used + n < stack.len() {
                    stack[used..used + n].copy_from_slice(bytes);
                    stack[used + n] = 0;
                    used += n + 1;
                    stack[used - n - 1..].as_ptr() as u64
                } else {
                    let mut v = Vec::with_capacity(n + 1);
                    v.extend_from_slice(bytes);
                    v.push(0);
                    long.push(v);
                    long.last().map_or(0, |v| v.as_ptr() as u64)
                };
                ni += 1;
            }
            Ty::Double => {
                floats[nf] = ctx.check_f64(k)?;
                nf += 1;
            }
            Ty::Float => {
                // A float travels in the low half of its vector register.
                let x = ctx.check_f64(k)? as f32;
                floats[nf] = f64::from_bits(u64::from(x.to_bits()));
                nf += 1;
            }
            Ty::Void => unreachable!("rejected by ffi.fn"),
        }
    }
    let ret = sig.ret();
    let v = if ret.is_float() {
        let r = sys::call_float(f, &ints, &floats);
        if ret == Ty::Float {
            Value::Float(f64::from(f32::from_bits(r.to_bits() as u32)))
        } else {
            Value::Float(r)
        }
    } else {
        let r = if sig.fast() {
            sys::call_int3(f, ints[0], ints[1], ints[2])
        } else {
            sys::call_int(f, &ints, &floats)
        };
        match ret {
            Ty::Void => Value::Nil,
            Ty::Int => Value::Int(i64::from(r as u32 as i32)),
            Ty::UInt => Value::Int(i64::from(r as u32)),
            Ty::Long | Ty::ULong => Value::Int(r as i64),
            Ty::Ptr => {
                if r == 0 {
                    Value::Nil
                } else {
                    light(r)
                }
            }
            Ty::CStr => match sys::cstr_bytes(r) {
                None => Value::Nil,
                Some(b) => ctx.new_str(b),
            },
            Ty::Double | Ty::Float => unreachable!(),
        }
    };
    drop(long);
    if ret == Ty::Void {
        Ok(NativeReturn::Return(0))
    } else {
        ctx.ret1(v)
    }
}

/// The arguments of a fast-trampoline call when they need no conversion
/// or checks beyond their tag (integers, pointers, nil as NULL).
#[inline]
fn fast_args(ctx: &NativeCtx<'_>, sig: Sig) -> Option<[u64; 3]> {
    let mut a = [0u64; 3];
    for (k, slot) in a.iter_mut().enumerate().take(sig.nargs()) {
        *slot = match (sig.arg(k), ctx.arg(k)) {
            (Ty::Int | Ty::UInt | Ty::Long | Ty::ULong, Value::Int(n)) => n as u64,
            (Ty::Ptr, Value::LightUserData(p)) => p.get(),
            (Ty::Ptr, Value::Nil) => 0,
            _ => return None,
        };
    }
    Some(a)
}

fn int_result(ctx: &mut NativeCtx<'_>, ret: Ty, r: u64) -> Result<NativeReturn, LuaError> {
    let v = match ret {
        Ty::Void => return Ok(NativeReturn::Return(0)),
        Ty::Int => Value::Int(i64::from(r as u32 as i32)),
        Ty::UInt => Value::Int(i64::from(r as u32)),
        Ty::Long | Ty::ULong => Value::Int(r as i64),
        Ty::Ptr => {
            if r == 0 {
                Value::Nil
            } else {
                light(r)
            }
        }
        Ty::CStr => match sys::cstr_bytes(r) {
            None => Value::Nil,
            Some(b) => ctx.new_str(b),
        },
        Ty::Double | Ty::Float => unreachable!("float results take the generic path"),
    };
    ctx.ret1(v)
}

fn ffi_errno(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    ctx.ret1(Value::Int(i64::from(sys::errno())))
}

fn ffi_buffer(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.check_int(0)?;
    if !(0..=(1 << 30)).contains(&n) {
        return Err(ctx.arg_error(0, "size out of range"));
    }
    let b = Buffer((0..n.max(1)).map(|_| Cell::new(0)).collect());
    let u = ctx.new_userdata(b);
    ctx.ret1(Value::UserData(u))
}

/// `ffi.string(p [, len])`: bytes from a buffer (bounds-checked) or from
/// a raw pointer (trusted; without `len`, up to the NUL).
fn ffi_string(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let len = match ctx.arg(1) {
        Value::Nil => None,
        _ => Some(ctx.check_int(1)?),
    };
    let bytes = match ctx.arg(0) {
        Value::UserData(u) => {
            let Some(b) = ctx.userdata_ref::<Buffer>(u) else {
                return Err(ctx.type_error(0, "pointer"));
            };
            match len {
                Some(n) if n < 0 || n as usize > b.0.len() => {
                    return Err(ctx.arg_error(1, "length out of buffer bounds"));
                }
                Some(n) => b.0[..n as usize].iter().map(Cell::get).collect(),
                None => b.0.iter().map(Cell::get).take_while(|&c| c != 0).collect(),
            }
        }
        Value::LightUserData(p) => match len {
            Some(n) if n >= 0 => sys::read(p.get(), n as usize),
            Some(_) => return Err(ctx.arg_error(1, "negative length")),
            None => sys::cstr_bytes(p.get()).unwrap_or(&[]).to_vec(),
        },
        _ => return Err(ctx.type_error(0, "pointer")),
    };
    let v = ctx.new_str(&bytes);
    ctx.ret1(v)
}

/// Baseline for benchmarks: a plain Rust native doing what `labs` does.
fn native_labs(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let n = ctx.check_int(0)?;
    ctx.ret1(Value::Int(n.wrapping_abs()))
}

/// Baseline for benchmarks: a native closure shaped like an ffi function
/// (two hidden upvalues, read on every call) whose body is Rust's `abs`.
fn closure_labs(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let hidden = [light(0), Value::Int(0)];
    let v = ctx.new_closure_hidden("ffi closure_labs", closure_labs_body, &[], &hidden);
    ctx.ret1(v)
}

fn closure_labs_body(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let (Value::LightUserData(_), Value::Int(_)) = (ctx.hidden_upvalue(0), ctx.hidden_upvalue(1))
    else {
        unreachable!()
    };
    let n = ctx.check_int(0)?;
    ctx.ret1(Value::Int(n.wrapping_abs()))
}

/// Baseline for benchmarks: a plain Rust native doing what `strlen` does.
fn native_strlen(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let s = ctx.check_str(0)?;
    let n = ctx
        .bytes(s)
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(ctx.bytes(s).len());
    ctx.ret1(Value::Int(n as i64))
}
