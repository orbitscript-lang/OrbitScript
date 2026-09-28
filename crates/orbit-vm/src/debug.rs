//! Names for values and functions in error messages, recovered from bytecode
//! (`ldebug.c`), and stack tracebacks.

use crate::state::{Frame, Vm, flag};
use orbit_bytecode::*;
use orbit_core::object::Proto;
use orbit_core::{Tm, Value};

/// Where an operand of a failed operation came from, for error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operand {
    /// Absolute stack slot.
    Slot(usize),
    /// Upvalue of the running Lua function.
    Upval(usize),
    /// A constant or computed value: no name.
    None,
}

/// A name for a value: kind (`"local"`, `"global"`, ...) and name.
pub(crate) type ObjName = (&'static str, String);

fn kname(p: &Proto, heap: &orbit_core::Heap, index: usize) -> Option<ObjName> {
    match p.constants.get(index) {
        Some(Value::Str(s)) => Some((
            "constant",
            String::from_utf8_lossy(heap.string(*s).as_bytes()).into_owned(),
        )),
        _ => None,
    }
}

fn kname_or_q(p: &Proto, heap: &orbit_core::Heap, index: usize) -> String {
    kname(p, heap, index).map_or_else(|| "?".to_string(), |(_, n)| n)
}

fn upval_name(p: &Proto, uv: usize) -> String {
    match p.info.upvalues.get(uv) {
        Some(d) if !d.name.is_empty() => d.name.to_string(),
        _ => "?".to_string(),
    }
}

/// `findsetreg`: the last instruction before `lastpc` that changed `reg`.
fn find_set_reg(code: &[u32], mut lastpc: usize, reg: usize) -> Option<usize> {
    if OpCode::of(code[lastpc]).is_mm() {
        lastpc = lastpc.saturating_sub(1);
    }
    let mut setreg = None;
    let mut jmptarget = 0usize;
    for (pc, &i) in code.iter().enumerate().take(lastpc) {
        let op = OpCode::of(i);
        let a = arg_a(i) as usize;
        let change = match op {
            OpCode::LoadNil => a <= reg && reg <= a + arg_b(i) as usize,
            OpCode::TForCall => reg >= a + 2,
            OpCode::Call | OpCode::TailCall => reg >= a,
            OpCode::Jmp => {
                let dest = pc as i64 + 1 + i64::from(arg_sj(i));
                if dest <= lastpc as i64 && dest > jmptarget as i64 {
                    jmptarget = dest as usize;
                }
                false
            }
            _ => op.sets_a() && reg == a,
        };
        if change {
            setreg = if pc < jmptarget { None } else { Some(pc) };
        }
    }
    setreg
}

impl Vm {
    /// `basicgetobjname`.
    fn basic_obj_name(&self, p: &Proto, pc: &mut Option<usize>, reg: usize) -> Option<ObjName> {
        let cur = (*pc)?;
        if let Some(name) = p.info.local_name((reg + 1) as u32, cur as u32) {
            return Some(("local", name.to_string()));
        }
        *pc = find_set_reg(&p.info.code, cur, reg);
        let setpc = (*pc)?;
        let i = p.info.code[setpc];
        match OpCode::of(i) {
            OpCode::Move => {
                let b = arg_b(i) as usize;
                if b < arg_a(i) as usize {
                    return self.basic_obj_name(p, pc, b);
                }
            }
            OpCode::GetUpval => return Some(("upvalue", upval_name(p, arg_b(i) as usize))),
            OpCode::LoadK => return kname(p, &self.heap, arg_bx(i) as usize),
            OpCode::LoadKX => {
                return kname(p, &self.heap, arg_ax(p.info.code[setpc + 1]) as usize);
            }
            _ => {}
        }
        None
    }

    /// `isEnv`: whether the table indexed by `i` is `_ENV`.
    fn is_env(&self, p: &Proto, pc: usize, i: u32, isup: bool) -> &'static str {
        let t = arg_b(i) as usize;
        let name = if isup {
            Some(upval_name(p, t))
        } else {
            match self.basic_obj_name(p, &mut Some(pc), t) {
                Some(("local", n)) | Some(("upvalue", n)) => Some(n),
                _ => None,
            }
        };
        if name.as_deref() == Some("_ENV") {
            "global"
        } else {
            "field"
        }
    }

    /// `getobjname`: a name for register `reg` at instruction `lastpc`.
    pub(crate) fn obj_name(&self, p: &Proto, lastpc: usize, reg: usize) -> Option<ObjName> {
        let mut pc = Some(lastpc);
        if let Some(n) = self.basic_obj_name(p, &mut pc, reg) {
            return Some(n);
        }
        let pc = pc?;
        let i = p.info.code[pc];
        match OpCode::of(i) {
            OpCode::GetTabUp => {
                let name = kname_or_q(p, &self.heap, arg_c(i) as usize);
                Some((self.is_env(p, pc, i, true), name))
            }
            OpCode::GetTable | OpCode::GetVarg => {
                // rname: only constant names count
                let k = arg_c(i) as usize;
                let name = match self.basic_obj_name(p, &mut Some(pc), k) {
                    Some(("constant", n)) => n,
                    _ => "?".to_string(),
                };
                Some((self.is_env(p, pc, i, false), name))
            }
            OpCode::GetI => Some(("field", "integer index".to_string())),
            OpCode::GetField => {
                let name = kname_or_q(p, &self.heap, arg_c(i) as usize);
                Some((self.is_env(p, pc, i, false), name))
            }
            OpCode::SelfOp => Some(("method", kname_or_q(p, &self.heap, arg_c(i) as usize))),
            _ => None,
        }
    }

    /// `funcnamefromcode`: how the function called by instruction `pc` of `p` got its
    /// name.
    pub(crate) fn func_name_from_code(&self, p: &Proto, pc: usize) -> Option<ObjName> {
        let i = p.info.code[pc];
        let tm = match OpCode::of(i) {
            OpCode::Call | OpCode::TailCall => return self.obj_name(p, pc, arg_a(i) as usize),
            OpCode::TForCall => return Some(("for iterator", "for iterator".to_string())),
            OpCode::SelfOp
            | OpCode::GetTabUp
            | OpCode::GetTable
            | OpCode::GetI
            | OpCode::GetField => Tm::Index,
            OpCode::SetTabUp | OpCode::SetTable | OpCode::SetI | OpCode::SetField => Tm::NewIndex,
            OpCode::MmBin | OpCode::MmBinI | OpCode::MmBinK => Tm::ALL[arg_c(i) as usize],
            OpCode::Unm => Tm::Unm,
            OpCode::BNot => Tm::BNot,
            OpCode::Len => Tm::Len,
            OpCode::Concat => Tm::Concat,
            OpCode::Eq => Tm::Eq,
            OpCode::Lt | OpCode::LtI | OpCode::GtI => Tm::Lt,
            OpCode::Le | OpCode::LeI | OpCode::GeI => Tm::Le,
            OpCode::Close | OpCode::Return => Tm::Close,
            _ => return None,
        };
        Some(("metamethod", tm.name()[2..].to_string()))
    }

    /// `funcnamefromcall`: a name for the function running in frame `callee_index`,
    /// from the frame that called it.
    pub(crate) fn func_name_for_frame(&self, callee_index: usize) -> Option<ObjName> {
        let callee = &self.st.frames[callee_index];
        if callee.flags & flag::TAIL != 0 || callee_index == 0 {
            return None;
        }
        let caller = &self.st.frames[callee_index - 1];
        if caller.flags & flag::HOOKED != 0 {
            return Some(("hook", "?".to_string()));
        }
        if caller.flags & flag::FIN != 0 {
            return Some(("metamethod", "__gc".to_string()));
        }
        self.func_name_from_caller(caller)
    }

    fn func_name_from_caller(&self, caller: &Frame) -> Option<ObjName> {
        if !caller.is_lua() {
            return None;
        }
        let p = self.frame_proto(caller)?;
        self.func_name_from_code(p, caller.pc.saturating_sub(1))
    }

    /// The prototype of a Lua frame.
    pub(crate) fn frame_proto(&self, fr: &Frame) -> Option<&Proto> {
        match self.st.stack.get(fr.func) {
            Some(Value::LuaFn(cl)) => Some(self.heap.get(self.heap.get(*cl).proto)),
            _ => None,
        }
    }

    /// `varinfo`: `" (local 'x')"` and the like for an operand of the running Lua
    /// function, or empty.
    pub(crate) fn var_info(&self, o: Operand) -> String {
        let Some(fr) = self.st.frames.last() else {
            return String::new();
        };
        if !fr.is_lua() {
            return String::new();
        }
        let Some(p) = self.frame_proto(fr) else {
            return String::new();
        };
        let found = match o {
            Operand::Upval(u) => Some(("upvalue", upval_name(p, u))),
            Operand::Slot(s) if s >= fr.base && s < fr.top => {
                self.obj_name(p, fr.pc.saturating_sub(1), s - fr.base)
            }
            _ => None,
        };
        match found {
            Some((kind, name)) => format!(" ({kind} '{name}')"),
            None => String::new(),
        }
    }

    /// The type name of a value for messages, honoring `__name` (`luaT_objtypename`).
    pub fn type_name_of(&self, v: Value) -> String {
        if let Some(mt) = self.heap.metatable(v)
            && matches!(v, Value::Table(_) | Value::UserData(_))
            && let Value::Str(s) = self.heap.raw_get_name(mt, b"__name")
        {
            return String::from_utf8_lossy(self.heap.string(s).as_bytes()).into_owned();
        }
        v.type_name().to_string()
    }

    /// "attempt to <op> a <type> value (<varinfo>)" (`luaG_typeerror`).
    pub(crate) fn type_error(&mut self, v: Value, o: Operand, op: &str) -> crate::Error {
        let t = self.type_name_of(v);
        let info = self.var_info(o);
        self.rt_error(format!("attempt to {op} a {t} value{info}"))
    }

    /// `luaG_callerror`: calling a value that is not callable. `func` is its slot.
    pub(crate) fn call_error(&mut self, func: usize) -> crate::Error {
        let v = self.st.stack[func];
        let named = self
            .st
            .frames
            .last()
            .and_then(|caller| self.func_name_from_caller(caller));
        let extra = match named {
            Some((kind, name)) => format!(" ({kind} '{name}')"),
            None => self.var_info(Operand::Slot(func)),
        };
        let t = self.type_name_of(v);
        self.rt_error(format!("attempt to call a {t} value{extra}"))
    }

    /// Name of the local variable in stack slot `slot` of the running Lua function.
    pub(crate) fn local_name_at(&self, slot: usize) -> Option<String> {
        let fr = self.st.frames.last()?;
        if !fr.is_lua() || slot < fr.base {
            return None;
        }
        let p = self.frame_proto(fr)?;
        p.info
            .local_name((slot - fr.base + 1) as u32, fr.pc.saturating_sub(1) as u32)
            .map(|s| s.to_string())
    }

    /// Current line of a Lua frame, or -1.
    pub(crate) fn frame_line(&self, fr: &Frame) -> i64 {
        match self.frame_proto(fr) {
            Some(p) if fr.is_lua() => p.info.line(fr.pc.saturating_sub(1)).map_or(-1, i64::from),
            _ => -1,
        }
    }

    /// A traceback of coroutine `co` (which may be the running one), like
    /// `luaL_traceback` with another thread.
    pub fn thread_traceback(
        &mut self,
        co: orbit_core::value::ThreadRef,
        msg: Option<&[u8]>,
        level: usize,
    ) -> Vec<u8> {
        self.with_thread(co, |vm| vm.traceback(msg, level))
    }

    /// A stack traceback like `luaL_traceback`, starting at frame depth `level` (0 is
    /// the running function).
    pub fn traceback(&self, msg: Option<&[u8]>, level: usize) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(m) = msg {
            out.extend_from_slice(m);
            out.push(b'\n');
        }
        out.extend_from_slice(b"stack traceback:");
        let n = self.st.frames.len();
        // Long stacks show the first LEVELS1 and last LEVELS2 levels.
        const LEVELS1: usize = 10;
        const LEVELS2: usize = 11;
        let shown = n.saturating_sub(level);
        let skip = if shown > LEVELS1 + LEVELS2 + 1 {
            shown - LEVELS1 - LEVELS2
        } else {
            0
        };
        for (k, idx) in (0..shown).rev().enumerate() {
            if skip > 0 && k >= LEVELS1 && k < LEVELS1 + skip {
                if k == LEVELS1 {
                    // The reference reports one level fewer than it leaves out.
                    let n = skip - 1;
                    out.extend_from_slice(format!("\n\t...\t(skipping {n} levels)").as_bytes());
                }
                continue;
            }
            let fr = self.st.frames[idx];
            out.extend_from_slice(b"\n\t");
            if fr.is_lua() {
                let p = self.frame_proto(&fr);
                let src = p
                    .and_then(|p| p.source)
                    .map(|s| orbit_lua_syntax::chunk_id(self.heap.string(s).as_bytes()))
                    .unwrap_or_else(|| b"?".to_vec());
                out.extend_from_slice(&src);
                out.extend_from_slice(format!(":{}: in ", self.frame_line(&fr)).as_bytes());
                let name = self.func_name_for_frame(idx);
                let line = p.map_or(0, |p| p.info.line_defined);
                match name {
                    Some((kind, name)) => {
                        out.extend_from_slice(format!("{kind} '{name}'").as_bytes())
                    }
                    None if line == 0 => out.extend_from_slice(b"main chunk"),
                    None => match self.global_func_name(self.st.stack[fr.func]) {
                        Some(g) => out.extend_from_slice(format!("function '{g}'").as_bytes()),
                        None => {
                            out.extend_from_slice(b"function <");
                            out.extend_from_slice(&src);
                            out.extend_from_slice(format!(":{line}>").as_bytes());
                        }
                    },
                }
            } else {
                out.extend_from_slice(b"[C]: in ");
                let f = self.st.stack[fr.func];
                match self.func_name_for_frame(idx) {
                    Some((kind, name)) => {
                        out.extend_from_slice(format!("{kind} '{name}'").as_bytes())
                    }
                    None => match self.global_func_name(f) {
                        Some(g) => out.extend_from_slice(format!("function '{g}'").as_bytes()),
                        None => out.extend_from_slice(b"?"),
                    },
                }
            }
            if fr.flags & flag::TAIL != 0 {
                out.extend_from_slice(b"\n\t(...tail calls...)");
            }
        }
        out
    }
}
