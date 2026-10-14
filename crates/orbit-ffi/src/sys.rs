//! Everything unsafe in the FFI spike: the dynamic loader, the call
//! trampolines, and reading C memory.
//!
//! The trampolines rely on how the C calling conventions of 64-bit Unix
//! targets (System V x86_64 and AAPCS64) pass arguments: integer and pointer
//! arguments go in integer registers in order, floating-point arguments in
//! vector registers in order, each class counted on its own. So one function
//! type with six integer and eight float parameters can call any
//! non-variadic C function whose arguments all fit in registers: the callee
//! reads the registers it declares and ignores the rest. Windows x64 counts
//! positions, not classes, and would need per-position variants.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void};

/// Integer-class argument registers the trampolines fill.
pub const INT_REGS: usize = 6;
/// Float-class argument registers the trampolines fill.
pub const FLOAT_REGS: usize = 8;

#[cfg(unix)]
mod dl {
    use std::ffi::{c_char, c_int, c_void};
    pub const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "linux")]
    pub const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
    #[cfg(target_os = "macos")]
    pub const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    unsafe extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlerror() -> *const c_char;
    }
}

fn dl_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let p = unsafe { dl::dlerror() };
    String::from_utf8_lossy(cstr_bytes(p as u64).unwrap_or(b"unknown error")).into_owned()
}

/// Open a library; `None` names the symbols already in the process.
pub fn open(name: Option<&[u8]>) -> Result<u64, String> {
    let Some(name) = name else {
        return Ok(dl::RTLD_DEFAULT as u64);
    };
    let c = std::ffi::CString::new(name).map_err(|_| "name contains a NUL byte".to_string())?;
    // SAFETY: loading a library runs its initializers; the host granted FFI.
    let h = unsafe { dl::dlopen(c.as_ptr(), dl::RTLD_NOW) };
    if h.is_null() {
        Err(dl_error())
    } else {
        Ok(h as u64)
    }
}

/// The address of `sym` in library handle `lib`.
pub fn symbol(lib: u64, sym: &[u8]) -> Result<u64, String> {
    let c = std::ffi::CString::new(sym).map_err(|_| "name contains a NUL byte".to_string())?;
    // SAFETY: `lib` came from `open`; the name is NUL-terminated.
    let f = unsafe { dl::dlsym(lib as *mut c_void, c.as_ptr()) };
    if f.is_null() {
        Err(dl_error())
    } else {
        Ok(f as u64)
    }
}

type Generic<R> =
    unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, f64, f64, f64, f64, f64, f64, f64, f64) -> R;
type Int3 = unsafe extern "C" fn(u64, u64, u64) -> u64;

/// Call `f` with every argument register filled; the integer result
/// register comes back.
pub fn call_int(f: u64, i: &[u64; INT_REGS], d: &[f64; FLOAT_REGS]) -> u64 {
    // SAFETY: none in general: the script declared `f`'s signature and
    // the host trusted it to (see the module comment for the convention).
    unsafe {
        let f: Generic<u64> = std::mem::transmute(f as usize);
        f(
            i[0], i[1], i[2], i[3], i[4], i[5], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        )
    }
}

/// [`call_int`] reading the float result register.
pub fn call_float(f: u64, i: &[u64; INT_REGS], d: &[f64; FLOAT_REGS]) -> f64 {
    // SAFETY: as for `call_int`.
    unsafe {
        let f: Generic<f64> = std::mem::transmute(f as usize);
        f(
            i[0], i[1], i[2], i[3], i[4], i[5], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7],
        )
    }
}

/// The fast trampoline: at most three integer-class arguments, none float.
pub fn call_int3(f: u64, a: u64, b: u64, c: u64) -> u64 {
    // SAFETY: as for `call_int`.
    unsafe {
        let f: Int3 = std::mem::transmute(f as usize);
        f(a, b, c)
    }
}

/// The fast float trampoline: at most three `double` arguments and result.
pub fn call_f3(f: u64, a: f64, b: f64, c: f64) -> f64 {
    // SAFETY: as for `call_int`.
    unsafe {
        let f: unsafe extern "C" fn(f64, f64, f64) -> f64 = std::mem::transmute(f as usize);
        f(a, b, c)
    }
}

/// The bytes of the NUL-terminated string at `p` (None for null).
pub fn cstr_bytes<'a>(p: u64) -> Option<&'a [u8]> {
    if p == 0 {
        return None;
    }
    // SAFETY: none: the script says `p` is a C string.
    Some(unsafe { std::ffi::CStr::from_ptr(p as *const c_char) }.to_bytes())
}

/// `len` bytes at `p`, copied.
pub fn read(p: u64, len: usize) -> Vec<u8> {
    // SAFETY: none: the script says `p` points at `len` readable bytes.
    unsafe { std::slice::from_raw_parts(p as *const u8, len) }.to_vec()
}

/// The calling thread's `errno`.
pub fn errno() -> c_int {
    #[cfg(target_os = "linux")]
    unsafe extern "C" {
        #[link_name = "__errno_location"]
        fn errno_ptr() -> *mut c_int;
    }
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        #[link_name = "__error"]
        fn errno_ptr() -> *mut c_int;
    }
    // SAFETY: both return the thread's errno cell.
    unsafe { *errno_ptr() }
}
