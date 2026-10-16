//! SPIKE: the hand-written boundary. `liborbit_table` exports one symbol,
//! `orbit_api_table`, returning this table; everything crosses as
//! `#[repr(C)]` data and `extern "C"` functions. Both sides wrap it.
use std::ffi::c_void;

pub const VERSION: u32 = 1;

pub const NIL: u8 = 0;
pub const BOOL: u8 = 1;
pub const INT: u8 = 2;
pub const FLOAT: u8 = 3;
pub const STR: u8 = 4;
pub const REF: u8 = 5;

/// A value at the boundary. `bits` holds a bool, integer, float's bits or
/// reference number; a string is `ptr`/`len`, borrowed: going in, for the
/// call; coming out, until the next call on the same state or context.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RawVal {
    pub tag: u8,
    pub bits: u64,
    pub ptr: *const u8,
    pub len: usize,
}

impl RawVal {
    pub const NIL: RawVal = RawVal {
        tag: NIL,
        bits: 0,
        ptr: std::ptr::null(),
        len: 0,
    };
}

pub type StatePtr = *mut c_void;
pub type CtxPtr = *mut c_void;

/// A program's native, called through the library: returns the number of
/// results it pushed, or -1 after `set_error`.
pub type NativeTramp = unsafe extern "C" fn(ctx: CtxPtr, ops: *const CtxTable, data: usize) -> i64;

/// Operations on a native's context.
#[repr(C)]
pub struct CtxTable {
    pub nargs: unsafe extern "C" fn(CtxPtr) -> usize,
    pub arg: unsafe extern "C" fn(CtxPtr, usize, *mut RawVal),
    pub push: unsafe extern "C" fn(CtxPtr, *const RawVal),
    /// Call `f` with `args`; 0 and the result count in `*nres`, or 1 with
    /// the error message as result 0.
    pub call: unsafe extern "C" fn(CtxPtr, *const RawVal, *const RawVal, usize, *mut usize) -> i32,
    pub result: unsafe extern "C" fn(CtxPtr, usize, *mut RawVal),
    pub set_error: unsafe extern "C" fn(CtxPtr, *const u8, usize),
}

/// Everything a program can do with the runtime.
#[repr(C)]
pub struct ApiTable {
    pub version: u32,
    pub new_state: unsafe extern "C" fn(ffi: bool) -> StatePtr,
    pub free_state: unsafe extern "C" fn(StatePtr),
    /// 0 and the function in `*out`, or 1 with the message in `*out`.
    pub load:
        unsafe extern "C" fn(StatePtr, *const u8, usize, *const u8, usize, *mut RawVal) -> i32,
    /// As `CtxTable::call`.
    pub call:
        unsafe extern "C" fn(StatePtr, *const RawVal, *const RawVal, usize, *mut usize) -> i32,
    pub result: unsafe extern "C" fn(StatePtr, usize, *mut RawVal),
    pub set_global: unsafe extern "C" fn(StatePtr, *const u8, usize, *const RawVal),
    pub global: unsafe extern "C" fn(StatePtr, *const u8, usize, *mut RawVal),
    pub new_table: unsafe extern "C" fn(StatePtr, *mut RawVal),
    pub table_set: unsafe extern "C" fn(StatePtr, *const RawVal, *const RawVal, *const RawVal),
    pub release: unsafe extern "C" fn(StatePtr, u32),
    pub tostring: unsafe extern "C" fn(StatePtr, *const RawVal, *mut RawVal),
    pub register: unsafe extern "C" fn(StatePtr, *const u8, usize, NativeTramp, usize),
}
