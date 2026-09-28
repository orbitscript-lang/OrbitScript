//! Debug hooks (`luaD_hook`, `luaD_hookcall`, `rethook`, `luaG_traceexec`).
//!
//! Hooks are per coroutine. The dispatch loop checks the hook mask before each
//! instruction when line or count hooks are on; calls and returns check it too.

use crate::error::LuaResult;
use crate::state::{HookEvent, HookFn, MIN_STACK, Vm, flag, hook_mask};
use orbit_bytecode::FnInfo;
use orbit_core::value::ThreadRef;

impl Vm {
    /// Sets the running coroutine's hook (`lua_sethook`). `mask` is made of
    /// [`hook_mask`] bits; `count` is the instruction count for count events. A `None`
    /// hook or empty mask turns hooks off.
    pub fn set_hook(&mut self, func: Option<HookFn>, mask: u8, count: u32) {
        let (func, mask) = match func {
            Some(f) if mask != 0 => (Some(f), mask),
            _ => (None, 0),
        };
        let h = &mut self.st.hook;
        h.func = func;
        h.base_count = count;
        h.count = count;
        h.mask = mask;
    }

    /// The running coroutine's hook, mask and count (`lua_gethook` and friends).
    pub fn hook(&self) -> (Option<HookFn>, u8, u32) {
        let h = &self.st.hook;
        (h.func, h.mask, h.base_count)
    }

    /// Sets the hook of coroutine `co`, which may be the running one.
    pub fn set_thread_hook(&mut self, co: ThreadRef, func: Option<HookFn>, mask: u8, count: u32) {
        self.with_thread(co, |vm| vm.set_hook(func, mask, count));
    }

    /// The hook of coroutine `co`, which may be the running one.
    pub fn thread_hook(&mut self, co: ThreadRef) -> (Option<HookFn>, u8, u32) {
        self.with_thread(co, |vm| vm.hook())
    }

    /// Calls the hook for `event` with the top frame as the hooked function
    /// (`luaD_hook`). `ftransfer` and `ntransfer` describe the values a call or return
    /// transfers, for `getinfo`'s `r` option.
    pub(crate) fn call_hook(
        &mut self,
        event: HookEvent,
        line: i64,
        ftransfer: usize,
        ntransfer: usize,
    ) -> LuaResult<()> {
        let Some(f) = self.st.hook.func else {
            return Ok(());
        };
        if !self.st.hook.allow {
            return Ok(());
        }
        let fi = self.st.frames.len() - 1;
        let top = self.st.top;
        let ftop = self.st.frames[fi].top;
        if self.st.frames[fi].is_lua() && self.st.top < ftop {
            // Protect the whole activation record.
            self.st.top = ftop;
        }
        self.grow_stack(self.st.top + MIN_STACK)?;
        let fr = &mut self.st.frames[fi];
        fr.top = fr.top.max(self.st.top + MIN_STACK);
        fr.flags |= flag::HOOKED;
        self.st.hook.transfer = (ftransfer, ntransfer);
        self.st.hook.allow = false;
        let r = f(self, event, line);
        self.st.hook.allow = true;
        // An error unwinds the hooked frame, so only a normal return restores it.
        if r.is_ok() {
            let fr = &mut self.st.frames[fi];
            fr.top = ftop;
            fr.flags &= !flag::HOOKED;
            self.st.top = top;
        }
        r
    }

    /// The call hook for the Lua frame `fi`, which is starting (`luaD_hookcall`).
    pub(crate) fn hook_call(&mut self, fi: usize) -> LuaResult<()> {
        self.st.hook.oldpc = 0;
        if self.st.hook.mask & hook_mask::CALL != 0 {
            let fr = self.st.frames[fi];
            let event = if fr.flags & flag::TAIL != 0 {
                HookEvent::TailCall
            } else {
                HookEvent::Call
            };
            let nparams = self.frame_proto(&fr).map_or(0, |p| p.info.num_params);
            self.call_hook(event, -1, 1, usize::from(nparams))?;
        }
        Ok(())
    }

    /// The return hook for frame `fi`, returning the `n` values from slot `first`
    /// (`rethook`).
    pub(crate) fn ret_hook(&mut self, fi: usize, first: usize, n: usize) -> LuaResult<()> {
        if self.st.hook.mask & hook_mask::RET != 0 {
            let ftransfer = first - self.st.frames[fi].func;
            self.call_hook(HookEvent::Return, -1, ftransfer, n)?;
        }
        if fi > 0 && self.st.frames[fi - 1].is_lua() {
            self.st.hook.oldpc = self.st.frames[fi - 1].pc.saturating_sub(1);
        }
        Ok(())
    }

    /// Count and line events before instruction `npci` of Lua frame `fi`
    /// (`luaG_traceexec`).
    pub(crate) fn trace_exec(&mut self, fi: usize, npci: usize, info: &FnInfo) -> LuaResult<()> {
        let mask = self.st.hook.mask;
        if npci == 0 && info.is_vararg() {
            // VARARGPREP: hooks start after it.
            return Ok(());
        }
        let counthook = mask & hook_mask::COUNT != 0 && {
            let h = &mut self.st.hook;
            h.count = h.count.wrapping_sub(1);
            h.count == 0
        };
        if counthook {
            self.st.hook.count = self.st.hook.base_count;
        } else if mask & hook_mask::LINE == 0 {
            return Ok(());
        }
        debug_assert_eq!(fi, self.st.frames.len() - 1);
        if counthook {
            self.call_hook(HookEvent::Count, -1, 0, 0)?;
        }
        if mask & hook_mask::LINE != 0 {
            let oldpc = if self.st.hook.oldpc < info.code.len() {
                self.st.hook.oldpc
            } else {
                0
            };
            if npci <= oldpc || info.line(oldpc) != info.line(npci) {
                let line = info.line(npci).map_or(-1, i64::from);
                self.call_hook(HookEvent::Line, line, 0, 0)?;
            }
            self.st.hook.oldpc = npci;
        }
        Ok(())
    }
}
