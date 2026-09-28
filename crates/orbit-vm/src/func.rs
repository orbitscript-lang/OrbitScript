//! Upvalues and to-be-closed variables (`lfunc.c`).

use crate::error::{Error, LuaResult};
use crate::state::Vm;
use orbit_core::object::{UpVal, UpValRef};
use orbit_core::{Tm, Value};

impl Vm {
    /// The open upvalue for stack slot `slot` of the running coroutine, created if
    /// needed (`luaF_findupval`).
    pub(crate) fn find_upval(&mut self, slot: usize) -> UpValRef {
        let list = &self.st.open_upvals;
        match list.binary_search_by_key(&slot, |&(s, _)| s) {
            Ok(i) => list[i].1,
            Err(i) => {
                let uv = self.heap.alloc(UpVal::Open {
                    thread: self.cur,
                    slot: slot as u32,
                });
                self.st.open_upvals.insert(i, (slot, uv));
                uv
            }
        }
    }

    /// Reads an upvalue.
    #[inline]
    pub(crate) fn upval_get(&self, uv: UpValRef) -> Value {
        match *self.heap.get(uv) {
            UpVal::Closed(v) => v,
            UpVal::Open { thread, slot } => {
                if thread == self.cur {
                    self.st.stack[slot as usize]
                } else {
                    self.heap
                        .get(thread)
                        .stack
                        .get(slot as usize)
                        .copied()
                        .unwrap_or(Value::Nil)
                }
            }
        }
    }

    /// Writes an upvalue.
    #[inline]
    pub(crate) fn upval_set(&mut self, uv: UpValRef, v: Value) {
        match *self.heap.get(uv) {
            UpVal::Closed(_) => self.heap.update(uv, |u| *u = UpVal::Closed(v)),
            UpVal::Open { thread, slot } => {
                if thread == self.cur {
                    self.st.stack[slot as usize] = v;
                } else {
                    self.heap.update(thread, |t| {
                        if let Some(s) = t.stack.get_mut(slot as usize) {
                            *s = v;
                        }
                    });
                }
            }
        }
    }

    /// Closes the open upvalues at or above `level` (`luaF_closeupval`).
    pub(crate) fn close_upvals(&mut self, level: usize) {
        while let Some(&(slot, uv)) = self.st.open_upvals.last() {
            if slot < level {
                break;
            }
            self.st.open_upvals.pop();
            let v = self.st.stack[slot];
            self.heap.update(uv, |u| *u = UpVal::Closed(v));
        }
    }

    /// Marks stack slot `slot` as a to-be-closed variable (`luaF_newtbcupval`).
    pub(crate) fn new_tbc(&mut self, slot: usize) -> LuaResult<()> {
        let v = self.st.stack[slot];
        if v.is_falsy() {
            return Ok(());
        }
        if self.heap.tm_of(v, Tm::Close).is_nil() {
            let name = self.local_name_at(slot).unwrap_or_else(|| "?".into());
            return Err(self.rt_error(format!("variable '{name}' got a non-closable value")));
        }
        self.st.tbc.push(slot);
        Ok(())
    }

    /// Closes upvalues and to-be-closed variables at or above `level` after normal
    /// execution (`luaF_close` with `LUA_OK`). Closing calls happen above the current
    /// top; an error from one propagates, leaving the rest to the error unwinding.
    pub(crate) fn close(&mut self, level: usize) -> LuaResult<()> {
        self.close_upvals(level);
        while let Some(&slot) = self.st.tbc.last() {
            if slot < level {
                break;
            }
            self.st.tbc.pop();
            let obj = self.st.stack[slot];
            let tm = self.heap.tm_of(obj, Tm::Close);
            let func = self.st.top;
            self.ensure_stack(func + 3);
            self.st.stack[func] = tm;
            self.st.stack[func + 1] = obj;
            self.st.top = func + 2;
            self.call_close_method(func)?;
            self.st.top = func;
        }
        Ok(())
    }

    /// Closes upvalues and to-be-closed variables at or above `level` while handling
    /// error `err`: each closing method gets the error object, and an error in one
    /// replaces `err` (`luaD_closeprotected`). Frames above the current one must already
    /// be gone.
    pub(crate) fn close_with_error(&mut self, level: usize, err: Error) -> Error {
        self.close_protected(level, Some(err))
            .expect("an error stays an error")
    }

    /// Like [`Vm::close_with_error`], starting from normal status when `err` is `None`
    /// (closing methods then get `nil`). Returns the final error, if any.
    pub(crate) fn close_protected(
        &mut self,
        level: usize,
        mut err: Option<Error>,
    ) -> Option<Error> {
        self.close_upvals(level);
        while let Some(&slot) = self.st.tbc.last() {
            if slot < level {
                break;
            }
            self.st.tbc.pop();
            let obj = self.st.stack[slot];
            let tm = self.heap.tm_of(obj, Tm::Close);
            let func = slot + 1;
            self.ensure_stack(func + 3);
            self.st.stack[func] = tm;
            self.st.stack[func + 1] = obj;
            self.st.stack[func + 2] = err.as_ref().map_or(Value::Nil, |e| e.value);
            self.st.top = func + 3;
            let depth = self.st.frames.len();
            if let Err(e) = self.call_close_method(func) {
                self.st.frames.truncate(depth);
                err = Some(e);
            }
        }
        err
    }

    /// Calls the closing method at `func` (arguments above it) with no results, as a
    /// non-yieldable call.
    fn call_close_method(&mut self, func: usize) -> LuaResult<()> {
        self.call_raw(func, Some(0), true)
    }
}
