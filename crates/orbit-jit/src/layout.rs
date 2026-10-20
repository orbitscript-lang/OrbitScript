//! The pinned layouts generated code relies on to read tables in place:
//! [`orbit_runtime::object::TABLE_LAYOUT`] for the fields, plus the two
//! facts Rust doesn't let safe code state, checked here once at start-up:
//! which word of a `Box<[T]>` is the pointer, and how an empty arena slot
//! (`None::<Table>`) is marked. If either check fails, the backends skip
//! the inline paths and use the runtime's helpers as before.

use std::mem::{MaybeUninit, size_of};
use std::sync::OnceLock;

use orbit_runtime::object::func::FieldCache;
use orbit_runtime::object::{Closure, TABLE_LAYOUT, Table};
use orbit_runtime::vm::CALL_LAYOUT;

/// Byte offsets and constants for inline table access. Offsets into an
/// arena slot are from the slot's start.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Inline {
    pub slot_size: i64,
    /// The array part's slot pointer and capacity (`usize`), and length (`u32`).
    pub array_ptr: i32,
    pub array_cap: i32,
    pub array_len: i32,
    pub array_slot_size: i64,
    pub array_slot_tag: i32,
    /// The hash part's entry pointer and count (`usize`).
    pub entries_ptr: i32,
    pub entries_len: i32,
    pub absent_meta: i32,
    pub entry_size: i64,
    pub entry_value_payload: i32,
    pub entry_key_payload: i32,
    pub entry_value_tag: i32,
    pub entry_key_tag: i32,
    /// An empty slot holds `empty_value` in the `empty_width` bytes at
    /// `empty_at`, and an occupied one never does.
    pub empty_at: i32,
    pub empty_width: u8,
    pub empty_value: u64,
    /// [`FieldCache::slot`] (`u32`).
    pub cache_slot: i32,
}

/// The layout, or `None` when it can't be pinned on this build.
pub(crate) fn inline() -> Option<&'static Inline> {
    static INLINE: OnceLock<Option<Inline>> = OnceLock::new();
    INLINE
        .get_or_init(|| {
            if std::env::var_os("ORBIT_JIT_NO_INLINE").is_some() {
                return None;
            }
            probe()
        })
        .as_ref()
}

fn probe() -> Option<Inline> {
    let l = TABLE_LAYOUT;
    let (ptr_word, len_word) = box_words()?;
    let (empty_at, empty_width, empty_value) = empty_marker::<Table>()?;
    Some(Inline {
        slot_size: l.slot_size as i64,
        array_ptr: (l.array_slots + ptr_word) as i32,
        array_cap: (l.array_slots + len_word) as i32,
        array_len: l.array_len as i32,
        array_slot_size: l.array_slot_size as i64,
        array_slot_tag: l.array_slot_tag as i32,
        entries_ptr: (l.entries + ptr_word) as i32,
        entries_len: (l.entries + len_word) as i32,
        absent_meta: l.absent_meta as i32,
        entry_size: l.entry_size as i64,
        entry_value_payload: l.entry_value_payload as i32,
        entry_key_payload: l.entry_key_payload as i32,
        entry_value_tag: l.entry_value_tag as i32,
        entry_key_tag: l.entry_key_tag as i32,
        empty_at: empty_at as i32,
        empty_width,
        empty_value,
        cache_slot: std::mem::offset_of!(FieldCache, slot) as i32,
    })
}

/// Byte offsets of the pointer and the length within a `Box<[T]>`.
fn box_words() -> Option<(usize, usize)> {
    let b: Box<[u64]> = vec![0u64; 3].into_boxed_slice();
    if size_of::<Box<[u64]>>() != 16 {
        return None;
    }
    // SAFETY: a boxed slice is two initialized words; reading them as
    // integers is a plain copy.
    let words: [usize; 2] = unsafe { std::mem::transmute_copy(&b) };
    let p = b.as_ptr() as usize;
    match words {
        [w0, 3] if w0 == p => Some((0, 8)),
        [3, w1] if w1 == p => Some((8, 0)),
        _ => None,
    }
}

/// How `None::<T>` is marked in an arena slot: the bytes it writes are the
/// ones that come out the same whatever the slot held before. A niche
/// value is by definition one no `Some` can hold.
fn empty_marker<T>() -> Option<(usize, u8, u64)> {
    let n = size_of::<Option<T>>();
    let fill = |byte: u8| -> Vec<u8> {
        let mut slot = MaybeUninit::<Option<T>>::uninit();
        // SAFETY: the slot is ours and `n` bytes long; writing `None` over
        // the filled bytes is an ordinary write of a valid value. Reading
        // back bytes `None` doesn't define is reading our own fill pattern
        // (a probe of how this build lays the enum out, nothing relies on
        // their values).
        unsafe {
            std::ptr::write_bytes(slot.as_mut_ptr() as *mut u8, byte, n);
            std::ptr::write(slot.as_mut_ptr(), None);
            (0..n)
                .map(|i| std::ptr::read_volatile((slot.as_ptr() as *const u8).add(i)))
                .collect()
        }
    };
    let (a, b) = (fill(0xAA), fill(0x55));
    let set: Vec<usize> = (0..n).filter(|&i| a[i] == b[i]).collect();
    let (&first, &last) = (set.first()?, set.last()?);
    let width = last - first + 1;
    if set.len() != width || !matches!(width, 1 | 2 | 4 | 8) || first % width != 0 {
        return None;
    }
    let mut v = [0u8; 8];
    v[..width].copy_from_slice(&a[first..=last]);
    Some((first, width as u8, u64::from_le_bytes(v)))
}

/// Byte offsets of a `Vec`'s pointer, length and capacity.
fn vec_words() -> Option<(usize, usize, usize)> {
    let mut v: Vec<u64> = Vec::with_capacity(7);
    v.extend([1, 2, 3]);
    if size_of::<Vec<u64>>() != 24 || v.capacity() == v.len() {
        return None;
    }
    // SAFETY: a `Vec` is three initialized words; reading them as integers
    // is a plain copy.
    let words: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
    let find = |x: usize| words.iter().position(|&w| w == x).map(|i| i * 8);
    let (p, l, c) = (find(v.as_ptr() as usize)?, find(3)?, find(v.capacity())?);
    (p != l && l != c && p != c).then_some((p, l, c))
}

/// How far past the pointer an `Rc` holds its value starts.
fn rc_value_offset() -> Option<i32> {
    let rc = std::rc::Rc::new(0u64);
    // SAFETY: an `Rc` is one initialized pointer word.
    let word: usize = unsafe { std::mem::transmute_copy(&rc) };
    let off = (std::rc::Rc::as_ptr(&rc) as usize).checked_sub(word)?;
    (off <= 64).then_some(off as i32)
}

/// Offsets for inline Lua-to-Lua calls and returns.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Calls {
    pub regs_ptr: i32,
    pub regs_len: i32,
    pub cells_len: i32,
    pub frames_ptr: i32,
    pub frames_len: i32,
    pub frames_cap: i32,
    pub top: i32,
    pub max_stack: i32,
    pub frame_size: i64,
    pub frame_kind: i32,
    pub lua_kind_tag: u8,
    pub frame_closure: i32,
    pub frame_code_id: i32,
    pub frame_func: i32,
    pub frame_base: i32,
    pub frame_pc: i32,
    pub frame_cell_base: i32,
    pub frame_nvarargs: i32,
    pub frame_want: i32,
    pub frame_tail: i32,
    pub frame_extra: i32,
    pub frame_cdepth: i32,
    pub closure_slot_size: i64,
    /// The `Rc<ProtoCode>` word, and how far past it the `ProtoCode` is.
    pub closure_code: i32,
    pub rc_value: i32,
    pub empty_at: i32,
    pub empty_width: u8,
    pub empty_value: u64,
    pub proto_entry: i32,
    pub proto_nparams: i32,
    pub proto_is_vararg: i32,
    pub proto_nregs: i32,
    pub proto_ncells: i32,
    pub window: i64,
    pub max_depth: u32,
}

/// The call layout, or `None` when it can't be pinned on this build (or
/// handles carry generations, which a frame's `u32` closure slot can't).
pub(crate) fn calls() -> Option<&'static Calls> {
    static CALLS: OnceLock<Option<Calls>> = OnceLock::new();
    CALLS
        .get_or_init(|| {
            if std::env::var_os("ORBIT_JIT_NO_INLINE").is_some()
                || std::env::var_os("ORBIT_JIT_NO_INLINE_CALLS").is_some()
                || CALL_LAYOUT.checked_handles
            {
                return None;
            }
            let l = CALL_LAYOUT;
            let (vp, vl, vc) = vec_words()?;
            let (empty_at, empty_width, empty_value) = empty_marker::<Closure>()?;
            Some(Calls {
                regs_ptr: (l.regs + vp) as i32,
                regs_len: (l.regs + vl) as i32,
                cells_len: (l.cells + vl) as i32,
                frames_ptr: (l.frames + vp) as i32,
                frames_len: (l.frames + vl) as i32,
                frames_cap: (l.frames + vc) as i32,
                top: l.top as i32,
                max_stack: l.max_stack as i32,
                frame_size: l.frame_size as i64,
                frame_kind: l.frame_kind as i32,
                lua_kind_tag: l.lua_kind_tag,
                frame_closure: l.frame_closure as i32,
                frame_code_id: l.frame_code_id as i32,
                frame_func: l.frame_func as i32,
                frame_base: l.frame_base as i32,
                frame_pc: l.frame_pc as i32,
                frame_cell_base: l.frame_cell_base as i32,
                frame_nvarargs: l.frame_nvarargs as i32,
                frame_want: l.frame_want as i32,
                frame_tail: l.frame_tail as i32,
                frame_extra: l.frame_extra as i32,
                frame_cdepth: l.frame_cdepth as i32,
                closure_slot_size: l.closure_slot_size as i64,
                closure_code: l.closure_code as i32,
                rc_value: rc_value_offset()?,
                empty_at: empty_at as i32,
                empty_width,
                empty_value,
                proto_entry: l.proto_entry as i32,
                proto_nparams: l.proto_nparams as i32,
                proto_is_vararg: l.proto_is_vararg as i32,
                proto_nregs: l.proto_nregs as i32,
                proto_ncells: l.proto_ncells as i32,
                window: l.window as i64,
                max_depth: l.max_depth,
            })
        })
        .as_ref()
}

/// Offsets for inline reads of captured variables (`CapGet`, `CellGet`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cells {
    pub calls: &'static Calls,
    /// A closure's captures: slice pointer and length words.
    pub captures_ptr: i32,
    pub captures_len: i32,
    /// The `Vm`'s cell stack pointer word.
    pub stack_ptr: i32,
    pub handle_size: i64,
    pub cell_slot_size: i64,
    pub cell_value: i32,
    pub empty_at: i32,
    pub empty_width: u8,
    pub empty_value: u64,
}

/// The cell layout, or `None` when it can't be pinned (it builds on
/// [`calls`], so the same switches turn it off).
pub(crate) fn cells() -> Option<&'static Cells> {
    static CELLS: OnceLock<Option<Cells>> = OnceLock::new();
    CELLS
        .get_or_init(|| {
            let calls = calls()?;
            let l = CALL_LAYOUT;
            if l.handle_size != 4 {
                return None;
            }
            let (ptr_word, len_word) = box_words()?;
            let (vp, _, _) = vec_words()?;
            let (empty_at, empty_width, empty_value) =
                empty_marker::<orbit_runtime::object::Cell>()?;
            Some(Cells {
                calls,
                captures_ptr: (l.closure_captures + ptr_word) as i32,
                captures_len: (l.closure_captures + len_word) as i32,
                stack_ptr: (l.cells + vp) as i32,
                handle_size: l.handle_size as i64,
                cell_slot_size: l.cell_slot_size as i64,
                cell_value: l.cell_value as i32,
                empty_at: empty_at as i32,
                empty_width,
                empty_value,
            })
        })
        .as_ref()
}
