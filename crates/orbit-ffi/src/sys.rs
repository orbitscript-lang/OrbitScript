//! Everything unsafe in the `ffi` library (ADR-9): the dynamic loader,
//! calls into C through libffi, and reading and writing memory C handed out.
//!
//! # How calls are made
//!
//! libffi does the calling. When a script binds a function, [`Cif::new`]
//! asks libffi to prepare a call description for the signature (`ffi_cif`):
//! which registers and stack slots each argument goes in under the target's
//! calling convention. A call then hands libffi the function's address, the
//! description and one pointer per argument value ([`Cif::call`]).
//!
//! Argument values arrive here as 64-bit slots holding the value's bits: an
//! integer narrower than 64 bits or a `float` in the low bits. libffi reads
//! each argument through its pointer at the size of its type, so on a
//! big-endian target the narrow values are moved to the slot's first bytes
//! first. Results come back the same way: libffi widens integer results to
//! a full register, and stores a `float` in its first four bytes.
//!
//! Every function here trusts its caller about what the address is and
//! about memory: that trust is what a host grants by enabling the library.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::addr_of_mut;

use libffi_sys as ffi;

/// A C type as libffi sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CType {
    Void,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Pointer,
    Float,
    Double,
}

impl CType {
    /// Bytes a value of this type takes.
    fn size(self) -> usize {
        match self {
            CType::Void => 0,
            CType::I8 | CType::U8 => 1,
            CType::I16 | CType::U16 => 2,
            CType::I32 | CType::U32 | CType::Float => 4,
            CType::I64 | CType::U64 | CType::Pointer | CType::Double => 8,
        }
    }

    /// libffi's description of the type: one of its predefined statics,
    /// which it only reads.
    fn ffi_type(self) -> *mut ffi::ffi_type {
        // SAFETY: taking a static's address reads nothing.
        unsafe {
            match self {
                CType::Void => addr_of_mut!(ffi::ffi_type_void),
                CType::I8 => addr_of_mut!(ffi::ffi_type_sint8),
                CType::U8 => addr_of_mut!(ffi::ffi_type_uint8),
                CType::I16 => addr_of_mut!(ffi::ffi_type_sint16),
                CType::U16 => addr_of_mut!(ffi::ffi_type_uint16),
                CType::I32 => addr_of_mut!(ffi::ffi_type_sint32),
                CType::U32 => addr_of_mut!(ffi::ffi_type_uint32),
                CType::I64 => addr_of_mut!(ffi::ffi_type_sint64),
                CType::U64 => addr_of_mut!(ffi::ffi_type_uint64),
                CType::Pointer => addr_of_mut!(ffi::ffi_type_pointer),
                CType::Float => addr_of_mut!(ffi::ffi_type_float),
                CType::Double => addr_of_mut!(ffi::ffi_type_double),
            }
        }
    }
}

/// A prepared call description for one signature.
pub struct Cif {
    /// Boxed so its address stays put: libffi keeps no pointer to it, but
    /// it points into `arg_types`.
    cif: Box<ffi::ffi_cif>,
    /// The argument types `cif` points at, kept alive and unmoved with it.
    _arg_types: Box<[*mut ffi::ffi_type]>,
    ret: CType,
    args: Box<[CType]>,
}

impl std::fmt::Debug for Cif {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cif")
            .field("ret", &self.ret)
            .field("args", &self.args)
            .finish()
    }
}

/// Arguments up to this many are passed without allocating.
const STACK_ARGS: usize = 8;

impl Cif {
    /// Prepare calls to C functions taking `args` and returning `ret`, under
    /// the target's default calling convention.
    pub fn new(ret: CType, args: &[CType]) -> Result<Cif, String> {
        let mut arg_types: Box<[*mut ffi::ffi_type]> = args.iter().map(|t| t.ffi_type()).collect();
        let nargs = u32::try_from(args.len()).map_err(|_| "too many arguments".to_string())?;
        let mut cif: Box<ffi::ffi_cif> = Box::default();
        // SAFETY: `cif` and `arg_types` are valid and outlive every use of
        // `cif`, since both move into the returned value on the heap; the
        // types are libffi's own statics.
        let status = unsafe {
            ffi::ffi_prep_cif(
                &mut *cif,
                ffi::ffi_abi_FFI_DEFAULT_ABI,
                nargs,
                ret.ffi_type(),
                arg_types.as_mut_ptr(),
            )
        };
        if status != ffi::ffi_status_FFI_OK {
            return Err(format!(
                "libffi cannot describe this signature (status {status})"
            ));
        }
        Ok(Cif {
            cif,
            _arg_types: arg_types,
            ret,
            args: args.into(),
        })
    }

    /// Call the C function at `f` with `args`, one slot per argument as the
    /// module comment describes, and return the result's bits.
    pub fn call(&self, f: u64, args: &mut [u64]) -> u64 {
        assert_eq!(args.len(), self.args.len(), "one slot per argument");
        for (slot, t) in args.iter_mut().zip(self.args.iter()) {
            *slot = to_memory_order(*slot, *t);
        }
        let mut stack = [std::ptr::null_mut::<c_void>(); STACK_ARGS];
        let mut heap = Vec::new();
        let pointers: &mut [*mut c_void] = if args.len() <= STACK_ARGS {
            &mut stack[..args.len()]
        } else {
            heap.resize(args.len(), std::ptr::null_mut());
            &mut heap
        };
        for (p, slot) in pointers.iter_mut().zip(args.iter_mut()) {
            *p = (slot as *mut u64).cast();
        }
        // Eight bytes, the width libffi widens integer results to on
        // 64-bit targets, and the size of every result type here.
        let mut result = 0u64;
        // SAFETY: none in general: the script declared the signature and the
        // host trusted it to. What libffi itself needs holds: `self.cif`
        // describes `pointers.len()` arguments, each pointer is to a slot
        // of at least its type's size, `result` is large enough for the
        // result type, and libffi does not write to the description during
        // the call.
        unsafe {
            let f: unsafe extern "C" fn() = std::mem::transmute(f as usize);
            ffi::ffi_call(
                (&*self.cif as *const ffi::ffi_cif).cast_mut(),
                Some(f),
                (&mut result as *mut u64).cast(),
                pointers.as_mut_ptr(),
            );
        }
        match self.ret {
            // Stored in the first four bytes, not widened.
            CType::Float => from_memory_order(result, CType::Float),
            _ => result,
        }
    }
}

/// `bits` (a value of type `t` in the low bits) placed where a read of
/// `t`'s size from the slot's address finds it.
fn to_memory_order(bits: u64, t: CType) -> u64 {
    if cfg!(target_endian = "big") && (1..8).contains(&t.size()) {
        bits << (64 - 8 * t.size())
    } else {
        bits
    }
}

/// The reverse of [`to_memory_order`].
fn from_memory_order(bits: u64, t: CType) -> u64 {
    if cfg!(target_endian = "big") && (1..8).contains(&t.size()) {
        bits >> (64 - 8 * t.size())
    } else {
        bits
    }
}

#[cfg(unix)]
mod dl {
    use std::ffi::{c_char, c_int, c_void};
    pub const RTLD_NOW: c_int = 2;
    #[cfg(target_os = "macos")]
    pub const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    #[cfg(not(target_os = "macos"))]
    pub const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
    extern "C" {
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
    let f = unsafe { dl::dlsym(lib as *mut c_void, c.as_ptr()) };
    if f.is_null() {
        Err(dl_error())
    } else {
        Ok(f as u64)
    }
}

#[cfg(windows)]
mod dl {
    use std::ffi::{c_char, c_void};
    #[link(name = "kernel32")]
    extern "system" {
        pub fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        pub fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
        pub fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
}

/// Open a DLL (never freed); `None` stands for the program itself.
#[cfg(windows)]
pub fn open(name: Option<&[u8]>) -> Result<u64, String> {
    let h = match name {
        // SAFETY: a null name asks for the program's own module.
        None => unsafe { dl::GetModuleHandleA(std::ptr::null()) },
        Some(name) => {
            let c =
                std::ffi::CString::new(name).map_err(|_| "name contains a NUL byte".to_string())?;
            // SAFETY: as for dlopen: the host consented to native code.
            unsafe { dl::LoadLibraryA(c.as_ptr()) }
        }
    };
    if h.is_null() {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(h as u64)
    }
}

/// The address of `name` in the module `lib` came from [`open`].
#[cfg(windows)]
pub fn symbol(lib: u64, name: &[u8]) -> Result<u64, String> {
    let c = std::ffi::CString::new(name).map_err(|_| "name contains a NUL byte".to_string())?;
    // SAFETY: `lib` is a module handle `open` returned and the name is
    // NUL-terminated.
    let f = unsafe { dl::GetProcAddress(lib as *mut c_void, c.as_ptr()) };
    if f.is_null() {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(f as u64)
    }
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

/// The calling thread's C `errno`.
#[cfg(unix)]
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The calling thread's C `errno`, from the C runtime (Windows keeps it
/// apart from `GetLastError`).
#[cfg(windows)]
pub fn errno() -> i32 {
    extern "C" {
        fn _errno() -> *mut std::ffi::c_int;
    }
    // SAFETY: `_errno` returns the calling thread's errno location.
    unsafe { *_errno() }
}
