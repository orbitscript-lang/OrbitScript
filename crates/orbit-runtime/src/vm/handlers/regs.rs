//! The two pointers a handler is handed: the running frame's registers
//! ([`Regs`]) and its place in the running code ([`Ip`]).
//!
//! Their methods are safe to call, and rest on two rules the compiler
//! does not check. ADR-15 lists who establishes each.
//!
//! - **The stack does not move under a [`Regs`].** A `Regs` is made from
//!   the register stack when it covers the frame's whole window
//!   ([`Regs::of_frame`]), and nothing may grow, shrink or replace that
//!   stack while the `Regs` is in use. A handler that needs anything that
//!   might (a call into a native, a metamethod, a collection step) gives
//!   its `Regs` up first, with [`super::leave`], and the chain makes a new
//!   one afterwards.
//! - **The code is verified and alive.** An [`Ip`] starts inside the code
//!   of a function whose frame is on the stack. Every chunk passes the
//!   bytecode verifier before it is linked, so a jump stays inside the
//!   code and the code ends in a return.
#![allow(unsafe_code)]

use std::mem::MaybeUninit;

use crate::object::value::Payload;
use crate::object::{ProtoCode, Tag, Value};
use crate::vm::VmState;
use crate::vm::interp::{WINDOW, Window};
use orbit_bytecode::{Offset, Op, Reg};

/// The registers of the running frame: a pointer to the stack slot just
/// below the frame's base, where its function sits and where a single
/// result goes.
#[repr(transparent)]
pub(super) struct Regs(*mut Value);

impl Regs {
    /// The registers of the frame at `base`, after extending the stack to
    /// cover its window. Returns them with the stack's length.
    #[inline(always)]
    pub(super) fn of_frame(stack: &mut Vec<Value>, base: usize) -> (Regs, usize) {
        assert!(base >= 1, "a frame's function sits below its base");
        let _ = Window::take(stack, base);
        // SAFETY: `base - 1` is inside the stack, which now covers the
        // window at `base`.
        (
            Regs(unsafe { stack.as_mut_ptr().add(base - 1) }),
            stack.len(),
        )
    }

    /// The frame's registers, indexed by register number.
    #[inline(always)]
    pub(super) fn window(&mut self) -> Window<'_> {
        // SAFETY: the stack covers the window, which starts one slot above
        // the pointer (the first rule of this module).
        Window::of(unsafe { &mut *(self.0.add(1) as *mut [Value; WINDOW]) })
    }

    /// `R[dst] = R[src]`, by [`copy_value`].
    #[inline(always)]
    pub(super) fn copy(&mut self, dst: Reg, src: Reg) {
        // SAFETY: two registers of the window, which may be the same one.
        unsafe { copy_slot(self.0.add(1 + dst as usize), self.0.add(1 + src as usize)) }
    }

    /// The slot below the frame's base.
    #[inline(always)]
    pub(super) fn below_base(&mut self) -> &mut Value {
        // SAFETY: a frame's base is at least 1, so the slot is in the stack.
        unsafe { &mut *self.0 }
    }

    /// The slot `n` below the frame's base, counting from 1.
    ///
    /// # Safety
    /// The frame's base must be at least `n`.
    #[inline(always)]
    pub(super) unsafe fn slot_below(&mut self, n: usize) -> *mut Value {
        self.0.add(1).sub(n)
    }

    /// Register `r` as a pointer, for moves that leave the window.
    #[inline(always)]
    pub(super) fn slot(&mut self, r: usize) -> *mut Value {
        debug_assert!(r < WINDOW);
        // SAFETY: inside the window.
        unsafe { self.0.add(1 + r) }
    }

    /// The registers of a frame whose base is `n` slots above this one's.
    ///
    /// # Safety
    /// The stack must cover that frame's window.
    #[inline(always)]
    pub(super) unsafe fn frame_above(self, n: usize) -> Regs {
        Regs(self.0.add(n))
    }

    /// The registers of a frame whose base is `n` slots below this one's.
    ///
    /// # Safety
    /// A frame with that base must be on the stack below this one.
    #[inline(always)]
    pub(super) unsafe fn frame_below(self, n: usize) -> Regs {
        Regs(self.0.sub(n))
    }
}

/// A place in the running code: a pointer to an instruction.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(super) struct Ip(*const Op);

impl Ip {
    /// Instruction `pc` of `code`.
    #[inline(always)]
    pub(super) fn at(code: &ProtoCode, pc: usize) -> Ip {
        assert!(pc < code.code.len(), "a pc inside the code");
        // SAFETY: checked above.
        Ip(unsafe { code.code.as_ptr().add(pc) })
    }

    /// Instruction `pc` of the code at `code`.
    ///
    /// # Safety
    /// `code` must be alive and `pc` inside it.
    #[inline(always)]
    pub(super) unsafe fn in_code(code: *const ProtoCode, pc: usize) -> Ip {
        Ip((*code).code.as_ptr().add(pc))
    }

    /// The instruction here.
    #[inline(always)]
    pub(super) fn op(self) -> Op {
        // SAFETY: inside the running code (the second rule of this module).
        unsafe { *self.0 }
    }

    /// The instruction before this one. A handler is handed the place
    /// after its own instruction, so this is the instruction it handles:
    /// the general case reads it here instead of being passed it.
    #[inline(always)]
    pub(super) fn handled(self) -> Op {
        // SAFETY: only called on the `Ip` a handler was handed, which
        // `super::next` made with `after` from a place inside the code.
        unsafe { *self.0.sub(1) }
    }

    /// The instruction after this one.
    #[inline(always)]
    pub(super) fn after(self) -> Ip {
        // SAFETY: verified code ends in a return, so an instruction that
        // goes on has a successor.
        Ip(unsafe { self.0.add(1) })
    }

    /// The instruction `off` away.
    #[inline(always)]
    pub(super) fn jump(self, off: Offset) -> Ip {
        // SAFETY: verified code only jumps inside itself.
        Ip(unsafe { self.0.offset(off as isize) })
    }

    /// This instruction's index in the running code.
    #[inline(always)]
    pub(super) fn pc(self, rt: &VmState) -> usize {
        // SAFETY: both point into the running code.
        unsafe { self.0.offset_from(rt.first_op) as usize }
    }

    /// The pointer itself, to record as the start of the running code.
    #[inline(always)]
    pub(super) fn as_ptr(self) -> *const Op {
        self.0
    }
}

/// The running function's code, as the chain keeps it: without a
/// reference count, alive because the function's frame is on the stack.
#[inline(always)]
pub(super) fn running_code(rt: &VmState) -> &ProtoCode {
    // SAFETY: the code of the closure the running frame names, which the
    // collector keeps and which owns its code.
    unsafe { &*rt.code }
}

/// `*dst = *src`, as the tag byte, the byte a boolean lives in and the
/// payload word: no branch on the tag, and each load matches the size of
/// the store that last wrote it.
#[inline(always)]
pub(super) fn copy_value(dst: &mut Value, src: &Value) {
    let s = src as *const Value as *const MaybeUninit<u8>;
    let d = dst as *mut Value as *mut MaybeUninit<u8>;
    // SAFETY: `Value` is `repr(C, u8)`, 16 bytes: the tag at 0, a `bool`
    // at 1, every other payload at 8 (ADR-1). Bytes are copied as possibly
    // uninitialised, so padding is never read as a value.
    unsafe {
        *d = *s;
        *d.add(1) = *s.add(1);
        *(d.add(8) as *mut MaybeUninit<u64>) = *(s.add(8) as *const MaybeUninit<u64>);
    }
}

/// [`copy_value`] between two stack slots.
///
/// # Safety
/// Both must be slots of the register stack.
#[inline(always)]
pub(super) unsafe fn copy_slot(dst: *mut Value, src: *const Value) {
    let s = src as *const MaybeUninit<u8>;
    let d = dst as *mut MaybeUninit<u8>;
    *d = *s;
    *d.add(1) = *s.add(1);
    *(d.add(8) as *mut MaybeUninit<u64>) = *(s.add(8) as *const MaybeUninit<u64>);
}

/// `*dst` = the value a table slot holds as tag `tag` and payload `p`,
/// without a branch on the tag.
#[inline(always)]
pub(super) fn write_packed(dst: &mut Value, tag: Tag, p: Payload) {
    write_tagged(dst, tag as u8, p)
}

/// [`write_packed`] with the tag as the byte an array part stores.
#[inline(always)]
pub(super) fn write_tagged(dst: &mut Value, tag: u8, p: Payload) {
    let d = dst as *mut Value as *mut u8;
    // SAFETY: the layout `copy_value` describes. `tag` is the tag byte of
    // a value a table stored, `p` that value's payload, and a boolean's
    // payload is 0 or 1.
    unsafe {
        *d = tag;
        *d.add(1) = (p.bits() != 0) as u8;
        *(d.add(8) as *mut u64) = p.bits();
    }
}
