//! Everything unsafe in the `ffi` library (ADR-9): the dynamic loader, the
//! calls into C, and reading and writing memory C handed out.
//!
//! # How calls are made
//!
//! There is no libffi. On the targets this supports (64-bit x86 and ARM,
//! Unix: the System V x86_64 and AAPCS64 calling conventions) every
//! argument of a non-variadic function goes in a register while there are
//! at most six integer-class and eight floating-point arguments, and the two
//! classes use separate registers, each counted on its own. So:
//!
//! - all-integer signatures with an integer result call through a function
//!   type of exactly their arity, with every argument widened to 64 bits as
//!   the conventions do (`call_ints`);
//! - all-double signatures of up to three arguments likewise (`call_floats`);
//! - anything else (mixed classes, or `float`) calls through one type taking
//!   six integers and eight doubles (`call_mixed`): the callee reads the
//!   registers its own signature names and ignores the rest. A `float`
//!   travels in the low half of its vector register, so it is passed as a
//!   double whose low 32 bits are the float's.
//!
//! The last two rely on the platform convention rather than on Rust's rules
//! for calling through a function pointer, which require the exact type;
//! the function behind the pointer is opaque to the compiler, so nothing can
//! be optimized on that assumption. Windows x64 counts positions rather than
//! classes, and variadic functions, structs by value and stack arguments
//! follow other rules: [`SUPPORTED`] is false there and nothing here is
//! reached.
//!
//! Every function here trusts its caller about what the address is and
//! about memory: that trust is what a host grants by enabling the library.

#![allow(unsafe_code)]

/// Whether this target's calling convention is the one described above.
pub const SUPPORTED: bool = cfg!(all(
    unix,
    target_pointer_width = "64",
    any(target_arch = "x86_64", target_arch = "aarch64")
));

/// Integer-class argument registers both conventions have.
pub const INT_REGS: usize = 6;
/// Floating-point argument registers both conventions have.
pub const FLOAT_REGS: usize = 8;
/// Arguments of up to this many doubles take the exact-type path.
pub const EXACT_FLOATS: usize = 3;

#[cfg(unix)]
mod dl {
    use std::ffi::{c_char, c_int, c_void};
    pub const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "macos")]
    pub const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    #[cfg(not(target_os = "macos"))]
    pub const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
    unsafe extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlerror() -> *const c_char;
    }
}

#[cfg(unix)]
fn dl_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let p = unsafe { dl::dlerror() };
    match cstr_bytes(p as u64) {
        Some(b) => String::from_utf8_lossy(b).into_owned(),
        None => "unknown error".into(),
    }
}

/// Open a shared library (never closed: functions from it may live as long
/// as the VM); `None` stands for the symbols already in the process.
#[cfg(unix)]
pub fn open(name: Option<&[u8]>) -> Result<u64, String> {
    let Some(name) = name else {
        return Ok(dl::RTLD_DEFAULT as u64);
    };
    let c = std::ffi::CString::new(name).map_err(|_| "name contains a NUL byte".to_string())?;
    // SAFETY: loading a library runs its initializers; the host enabled the
    // library, which is consent to run native code.
    let h = unsafe { dl::dlopen(c.as_ptr(), dl::RTLD_NOW) };
    if h.is_null() {
        Err(dl_error())
    } else {
        Ok(h as u64)
    }
}

/// The address of `name` in the library `lib` came from [`open`].
#[cfg(unix)]
pub fn symbol(lib: u64, name: &[u8]) -> Result<u64, String> {
    let c = std::ffi::CString::new(name).map_err(|_| "name contains a NUL byte".to_string())?;
    // SAFETY: `lib` is a handle `open` returned and the name is
    // NUL-terminated. dlsym may legitimately return null for a symbol whose
    // value is null; that is no function, so it counts as missing.
    let f = unsafe { dl::dlsym(lib as *mut std::ffi::c_void, c.as_ptr()) };
    if f.is_null() {
        Err(dl_error())
    } else {
        Ok(f as u64)
    }
}

#[cfg(not(unix))]
pub fn open(_name: Option<&[u8]>) -> Result<u64, String> {
    Err("not supported on this platform".into())
}

#[cfg(not(unix))]
pub fn symbol(_lib: u64, _name: &[u8]) -> Result<u64, String> {
    Err("not supported on this platform".into())
}

/// A call's result register: the integer one, or the bits of the float one.
macro_rules! call_as {
    ($f:expr, $float:expr, ($($t:ty),*), ($($a:expr),*)) => {{
        if $float {
            // SAFETY: see the module comment: the script declared the
            // signature and the host trusted it to.
            let f: unsafe extern "C" fn($($t),*) -> f64 = unsafe { std::mem::transmute($f as usize) };
            unsafe { f($($a),*) }.to_bits()
        } else {
            // SAFETY: as above.
            let f: unsafe extern "C" fn($($t),*) -> u64 = unsafe { std::mem::transmute($f as usize) };
            unsafe { f($($a),*) }
        }
    }};
}

/// Call `f` with integer-class arguments only (at most [`INT_REGS`]) and an
/// integer-class result.
pub fn call_ints(f: u64, a: &[u64]) -> u64 {
    match *a {
        [] => call_as!(f, false, (), ()),
        [a0] => call_as!(f, false, (u64), (a0)),
        [a0, a1] => call_as!(f, false, (u64, u64), (a0, a1)),
        [a0, a1, a2] => call_as!(f, false, (u64, u64, u64), (a0, a1, a2)),
        [a0, a1, a2, a3] => call_as!(f, false, (u64, u64, u64, u64), (a0, a1, a2, a3)),
        [a0, a1, a2, a3, a4] => {
            call_as!(f, false, (u64, u64, u64, u64, u64), (a0, a1, a2, a3, a4))
        }
        [a0, a1, a2, a3, a4, a5] => call_as!(
            f,
            false,
            (u64, u64, u64, u64, u64, u64),
            (a0, a1, a2, a3, a4, a5)
        ),
        _ => unreachable!("signatures are checked when bound"),
    }
}

/// Call `f` with one to [`EXACT_FLOATS`] double arguments only.
pub fn call_floats(f: u64, a: &[f64], float: bool) -> u64 {
    match *a {
        [a0] => call_as!(f, float, (f64), (a0)),
        [a0, a1] => call_as!(f, float, (f64, f64), (a0, a1)),
        [a0, a1, a2] => call_as!(f, float, (f64, f64, f64), (a0, a1, a2)),
        _ => unreachable!("signatures are checked when bound"),
    }
}

/// Call `f` with every argument register filled (module comment).
pub fn call_mixed(f: u64, i: &[u64; INT_REGS], d: &[f64; FLOAT_REGS], float: bool) -> u64 {
    call_as!(
        f,
        float,
        (
            u64, u64, u64, u64, u64, u64, f64, f64, f64, f64, f64, f64, f64, f64
        ),
        (
            i[0], i[1], i[2], i[3], i[4], i[5], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    )
}

/// The bytes of the NUL-terminated string at `p`, or `None` for null.
pub fn cstr_bytes<'a>(p: u64) -> Option<&'a [u8]> {
    if p == 0 {
        return None;
    }
    // SAFETY: none in general: the script says `p` is a C string. The
    // caller copies the bytes before anything else runs.
    Some(unsafe { std::ffi::CStr::from_ptr(p as *const std::ffi::c_char) }.to_bytes())
}

/// Copy `out.len()` bytes from address `p`.
pub fn read(p: u64, out: &mut [u8]) {
    // SAFETY: none in general: the script says `p` points at that many
    // readable bytes.
    unsafe { std::ptr::copy_nonoverlapping(p as *const u8, out.as_mut_ptr(), out.len()) }
}

/// Copy `bytes` to address `p`.
pub fn write(p: u64, bytes: &[u8]) {
    // SAFETY: none in general: the script says `p` points at that many
    // writable bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) }
}

/// The calling thread's `errno`.
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
