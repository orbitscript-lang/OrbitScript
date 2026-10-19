//! The pinned layouts generated code relies on to read tables in place:
//! [`orbit_runtime::object::TABLE_LAYOUT`] for the fields, plus the two
//! facts Rust doesn't let safe code state, checked here once at start-up:
//! which word of a `Box<[T]>` is the pointer, and how an empty arena slot
//! (`None::<Table>`) is marked. If either check fails, the backends skip
//! the inline paths and use the runtime's helpers as before.

use std::mem::{MaybeUninit, size_of};
use std::sync::OnceLock;

use orbit_runtime::object::func::FieldCache;
use orbit_runtime::object::{TABLE_LAYOUT, Table};

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
    let (empty_at, empty_width, empty_value) = empty_marker()?;
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

/// How `None::<Table>` is marked: the bytes it writes are the ones that
/// come out the same whatever the slot held before.
fn empty_marker() -> Option<(usize, u8, u64)> {
    let n = size_of::<Option<Table>>();
    let fill = |byte: u8| -> Vec<u8> {
        let mut slot = MaybeUninit::<Option<Table>>::uninit();
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
    let value = u64::from_le_bytes(v);
    // An occupied slot must never carry the marker: check a fresh table.
    let some = Some(Table::new());
    let some_bytes: &[u8] = {
        // SAFETY: as above, a probe read of `width` bytes the enum's tag
        // occupies, which `Some` initializes (they are part of its field).
        unsafe { std::slice::from_raw_parts((&raw const some as *const u8).add(first), width) }
    };
    if some_bytes == &a[first..=last] {
        return None;
    }
    Some((first, width as u8, value))
}
