//! Introspection for the `debug` library, tracebacks and `error` levels
//! ([debug design]).
//!
//! The frame stack holds Lua frames, native frames and meta frames. The
//! debug library numbers *activations*: level 0 is the native that is
//! running (it usually has no frame), then every Lua or native frame from
//! the top down. Meta frames (and hook frames) are not levels; they name
//! the activation above them.
//!
//! [debug design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/debug.md

use std::fmt::Write as _;

use crate::object::{Cell, Closure, Gc, LuaHeap, NativeId, Proto, Table, Value};
use orbit_bytecode::{LocalSlot, Op, Origin};

use crate::vm::{FrameKind, Runtime, Stack};

/// The longest traceback printed in full; longer ones keep the first
/// [`TRACE_HEAD`] and last [`TRACE_TAIL`] levels (observed from `lua5.5`).
const TRACE_FULL: usize = 22;
const TRACE_HEAD: usize = 10;
const TRACE_TAIL: usize = 11;

/// The native that is running, when it has no frame of its own: its id and
/// the stack index of its function slot.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Running {
    pub id: NativeId,
    pub func: usize,
}

/// One activation on the stack, as the debug library numbers them, and
/// for the top frame under a running native without a frame, that
/// native's function slot (where the frame's registers end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level(LevelAt, Option<usize>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelAt {
    /// The running native, which has no frame. Its caller is the top
    /// frame.
    Running { id: NativeId, func: usize },
    /// Frame `i` of the stack.
    Frame(usize),
}

/// How an activation was called, for `getinfo`'s `name` and `namewhat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallName {
    /// `global`, `local`, `method`, `field`, `upvalue`, `constant`,
    /// `for iterator`, `metamethod` or `hook`.
    pub namewhat: &'static str,
    pub name: Vec<u8>,
}

/// What a local slot is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalName<'a> {
    Named(&'a [u8]),
    /// A register in use with no name (`(temporary)`).
    Temporary,
    /// A slot of a native's window (`(C temporary)`).
    CTemporary,
    /// An extra argument of a vararg function (`(vararg)`).
    Vararg,
    /// The hidden local of a vararg function with an unnamed `...`.
    VarargTable,
}

impl LocalName<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            LocalName::Named(n) => n,
            LocalName::Temporary => b"(temporary)",
            LocalName::CTemporary => b"(C temporary)",
            LocalName::Vararg => b"(vararg)",
            LocalName::VarargTable => b"(vararg table)",
        }
    }
}

/// Where a local's value lives.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Place {
    /// A stack slot.
    Reg(usize),
    /// A captured local's cell.
    Cell(Gc<Cell>),
    /// Always nil, never written (the unnamed vararg table).
    Nil,
}

/// Static facts about a function (`getinfo`'s `S` and `u` options).
#[derive(Debug, Clone)]
pub struct FuncInfo {
    /// `@file.lua`, `=stdin`, the source text, or `=[C]` for natives.
    pub source: Vec<u8>,
    /// The short form used in messages.
    pub short_src: String,
    /// `Lua`, `C` or `main`.
    pub what: &'static str,
    pub line_defined: i64,
    pub last_line: i64,
    pub nups: usize,
    pub nparams: u8,
    pub is_vararg: bool,
}

impl Runtime {
    /// How many hidden upvalues closure `c` has (see [`crate::vm::Native::hidden`]).
    /// A built-in library function hides all of its upvalues, as a C
    /// function with none would.
    pub(crate) fn hidden_upvalues(&self, heap: &LuaHeap, c: &Closure) -> usize {
        let p = heap.get(c.proto);
        match p.code.code.first() {
            Some(Op::NativeEntry { id }) => self.natives[*id as usize].hidden as usize,
            _ if p.code.builtin => c.captures.len(),
            _ => 0,
        }
    }

    /// Activation `n`. With a running native, level 0 is that native and
    /// level 1 its caller; without one, level 1 is the top frame.
    pub(crate) fn level(&self, running: Option<Running>, n: usize) -> Option<Level> {
        let frames = &self.stack.frames;
        let mut top = frames.len();
        if let Some(r) = running {
            // A native running a continuation, or any native while hooks
            // are on, has a frame.
            let own = matches!(frames.last(), Some(f)
                if matches!(f.kind, FrameKind::NativeCall { .. } | FrameKind::Native { .. })
                    && f.func as usize == r.func);
            if n == 0 {
                return Some(Level(
                    if own {
                        LevelAt::Frame(top - 1)
                    } else {
                        LevelAt::Running {
                            id: r.id,
                            func: r.func,
                        }
                    },
                    None,
                ));
            }
            if own {
                top -= 1;
            }
        } else if n == 0 {
            return None;
        }
        let below = running.filter(|_| top == frames.len()).map(|r| r.func);
        self.find_activation(top, n)
            .map(|i| Level(LevelAt::Frame(i), below.filter(|_| i + 1 == top)))
    }

    /// Activations `from`, `from + 1`, and so on, in one pass over the
    /// frames (asking `level` for each would be quadratic in the depth).
    pub(crate) fn levels(&self, running: Option<Running>, from: usize) -> Vec<Level> {
        let mut out = Vec::new();
        let mut next = self.level(running, from);
        if let Some(l @ Level(LevelAt::Running { .. }, _)) = next {
            out.push(l);
            next = self.level(running, from + 1);
        }
        while let Some(l @ Level(LevelAt::Frame(i), _)) = next {
            out.push(l);
            next = self
                .find_activation(self.scan_on(i), 1)
                .map(|i| Level(LevelAt::Frame(i), None));
        }
        out
    }

    /// The `left`th activation below frame `top`, as a frame index.
    fn find_activation(&self, top: usize, mut left: usize) -> Option<usize> {
        let mut i = top;
        while i > 0 {
            i -= 1;
            if is_activation(self.stack.frames[i].kind) {
                left -= 1;
                if left == 0 {
                    return Some(i);
                }
            }
            i = self.scan_on(i);
        }
        None
    }

    /// Where a scan down the frames continues after frame `i`.
    fn scan_on(&self, i: usize) -> usize {
        let frames = &self.stack.frames;
        if unwinding_close(frames[i].kind) {
            // The frames the error left are gone in the reference by the
            // time a `__close` handler runs: its caller is the frame that
            // catches the error.
            frames[..i]
                .iter()
                .rposition(|f| catches(f.kind))
                .map_or(0, |c| c + 1)
        } else {
            i
        }
    }

    /// Index of the frame below activation `l` that called it, if any.
    fn caller_index(&self, l: Level) -> Option<usize> {
        match l.0 {
            LevelAt::Running { .. } => self.stack.frames.len().checked_sub(1),
            LevelAt::Frame(i) => i.checked_sub(1),
        }
    }

    /// The function running at `l`.
    pub(crate) fn level_func(&self, heap: &LuaHeap, l: Level) -> Value {
        let _ = heap;
        match l.0 {
            LevelAt::Running { id, .. } => Value::Native(id),
            LevelAt::Frame(i) => frame_func(&self.stack, i),
        }
    }

    /// The current line of `l`, or -1 for natives and code without line
    /// information.
    pub(crate) fn current_line(&self, heap: &LuaHeap, l: Level) -> i64 {
        let LevelAt::Frame(i) = l.0 else { return -1 };
        let f = &self.stack.frames[i];
        match f.kind {
            FrameKind::Lua { closure, .. } => {
                let proto = heap.get(heap.get(closure).proto);
                pc_line(proto, f.pc.saturating_sub(1))
            }
            _ => -1,
        }
    }

    /// Whether `l` was entered by a tail call.
    pub(crate) fn is_tail_call(&self, l: Level) -> bool {
        matches!(l.0, LevelAt::Frame(i) if self.stack.frames[i].tail)
    }

    /// Extra arguments a `__call` chain gave `l`.
    pub(crate) fn extra_args(&self, l: Level) -> u8 {
        match l.0 {
            LevelAt::Frame(i) => self.stack.frames[i].extra,
            LevelAt::Running { .. } => 0,
        }
    }

    /// How `l` was called, from its caller's call instruction: `local 'f'`,
    /// `metamethod 'index'` and so on. `None` for tail calls and calls from
    /// natives.
    pub(crate) fn call_name(&self, heap: &LuaHeap, l: Level) -> Option<CallName> {
        if self.is_tail_call(l) {
            return None;
        }
        let ci = self.caller_index(l)?;
        let caller = &self.stack.frames[ci];
        let named = |namewhat: &'static str, name: &[u8]| {
            Some(CallName {
                namewhat,
                name: name.to_vec(),
            })
        };
        match caller.kind {
            // A `__close` handler run by an error is called like a plain
            // function.
            k if unwinding_close(k) => None,
            FrameKind::Meta { field, .. } => named("metamethod", field.short_name().as_bytes()),
            FrameKind::Hook => named("hook", b"?"),
            FrameKind::Boundary { .. } | FrameKind::Native { .. }
                if self.stack.aux.finalizer_at == Some(ci as u32) =>
            {
                named("metamethod", b"__gc")
            }
            // A built-in calls like a C function: its callees have no name.
            FrameKind::Lua { closure, .. } if caller.pc > 0 && !is_builtin(heap, closure) => {
                let proto = heap.get(heap.get(closure).proto);
                let pc = caller.pc - 1;
                match proto.code.code.get(pc as usize)? {
                    Op::GenericCall { .. } => named("for iterator", b"for iterator"),
                    Op::Call { .. } | Op::TailCall { .. } => {
                        let (namewhat, name) = match proto.debug.origin(pc, 0)? {
                            Origin::Global(n) => ("global", n),
                            Origin::Local(n) => ("local", n),
                            Origin::Field(n) => ("field", n),
                            Origin::Method(n) => ("method", n),
                            Origin::Upvalue(n) => ("upvalue", n),
                            Origin::Constant(n) => ("constant", n),
                        };
                        named(namewhat, name)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `chunk:line: ` for activation `n` (as [`Runtime::level`] numbers
    /// them), or empty if it isn't a Lua function or its line is unknown (a
    /// stripped chunk): the prefix `error` and natives' errors add.
    pub(crate) fn where_(&self, heap: &LuaHeap, running: Option<Running>, n: usize) -> String {
        self.position(heap, running, n, false)
    }

    /// The prefix of a runtime error raised by an instruction: like
    /// [`Runtime::where_`], but a stripped chunk gives `chunk:?: `, as in
    /// the reference.
    pub(crate) fn error_where(&self, heap: &LuaHeap, n: usize) -> String {
        self.position(heap, None, n, true)
    }

    fn position(
        &self,
        heap: &LuaHeap,
        running: Option<Running>,
        n: usize,
        unknown_line: bool,
    ) -> String {
        let Some(l) = self.level(running, n) else {
            return String::new();
        };
        let LevelAt::Frame(i) = l.0 else {
            return String::new();
        };
        let f = &self.stack.frames[i];
        match f.kind {
            FrameKind::Lua { closure, .. } if !is_builtin(heap, closure) => {
                let proto = heap.get(heap.get(closure).proto);
                let line = pc_line(proto, f.pc.saturating_sub(1));
                if line <= 0 {
                    if unknown_line {
                        format!("{}:?: ", proto.chunk)
                    } else {
                        String::new()
                    }
                } else {
                    format!("{}:{}: ", proto.chunk, line)
                }
            }
            _ => String::new(),
        }
    }

    // ---- locals ---------------------------------------------------------

    /// Local `n` of activation `l` (1-based; negative for varargs), with
    /// where its value lives.
    pub(crate) fn local_at<'h>(
        &self,
        heap: &'h LuaHeap,
        l: Level,
        n: i64,
    ) -> Option<(LocalName<'h>, Place)> {
        let (base, limit, kind) = match l.0 {
            LevelAt::Running { func, .. } => (func + 1, self.top, None),
            LevelAt::Frame(i) => {
                let f = &self.stack.frames[i];
                // Registers above the frame's are in use only up to the
                // function slot of whatever it is calling.
                let next = self.stack.frames[i + 1..]
                    .first()
                    .map(|g| g.func as usize)
                    .or(l.1);
                match f.kind {
                    FrameKind::Lua { closure, .. } => {
                        let c = heap.get(closure);
                        let limit = next.unwrap_or(f.base as usize + c.code.nregs as usize);
                        // A built-in's registers are a C function's window.
                        let named = (!c.code.builtin).then_some((c, f));
                        (f.base as usize, limit, named)
                    }
                    FrameKind::Native { call_at, .. } => {
                        (f.base as usize, next.unwrap_or(call_at as usize), None)
                    }
                    FrameKind::NativeCall { .. } => {
                        (f.base as usize, next.unwrap_or(self.top), None)
                    }
                    FrameKind::Meta { .. }
                    | FrameKind::Handler
                    | FrameKind::Boundary { .. }
                    | FrameKind::CoBase
                    | FrameKind::Hook => return None,
                }
            }
        };
        let Some((closure, frame)) = kind else {
            // A native: its window, unnamed.
            if n < 1 {
                return None;
            }
            let at = base + n as usize - 1;
            return (at < limit && at < self.stack.regs.len())
                .then_some((LocalName::CTemporary, Place::Reg(at)));
        };
        let proto = heap.get(closure.proto);
        if n < 0 {
            let k = n.unsigned_abs() as usize;
            let nvar = frame.nvarargs as usize;
            return (k <= nvar).then_some((LocalName::Vararg, Place::Reg(base - nvar + k - 1)));
        }
        if n == 0 {
            return None;
        }
        let pc = frame.pc.saturating_sub(1);
        let mut k = n as usize;
        let cell_base = frame.cell_base as usize;
        for (name, slot) in active_locals(proto, pc) {
            if k == 1 {
                let place = match slot {
                    Some(LocalSlot::Reg(r)) => Place::Reg(base + r as usize),
                    Some(LocalSlot::Cell(c)) => {
                        Place::Cell(self.stack.cells[cell_base + c as usize])
                    }
                    None => Place::Nil,
                };
                return Some((name, place));
            }
            k -= 1;
        }
        // Past the named locals: temporaries in use.
        let (_, first_free) = temporaries(proto, pc);
        let at = base + first_free + k - 1;
        (at < limit).then_some((LocalName::Temporary, Place::Reg(at)))
    }

    /// The value at `p`.
    pub(crate) fn read_place(&self, heap: &LuaHeap, p: Place) -> Value {
        match p {
            Place::Reg(i) => self.stack.regs.get(i).copied().unwrap_or(Value::Nil),
            Place::Cell(c) => heap.get(c).value,
            Place::Nil => Value::Nil,
        }
    }

    /// Store `v` at `p`.
    pub(crate) fn write_place(&mut self, heap: &mut LuaHeap, p: Place, v: Value) {
        match p {
            Place::Reg(i) => {
                if let Some(r) = self.stack.regs.get_mut(i) {
                    *r = v;
                }
            }
            Place::Cell(c) => {
                crate::vm::debug::before_cell_write(heap, c);
                heap.barrier_back(c);
                heap.get_mut(c).value = v;
            }
            Place::Nil => {}
        }
    }

    // ---- tracebacks -----------------------------------------------------

    /// The traceback text from activation `from` down (`stack traceback:`
    /// and one line per level), in the reference's format.
    pub(crate) fn traceback(
        &self,
        heap: &LuaHeap,
        running: Option<Running>,
        from: usize,
    ) -> String {
        let levels = self.levels(running, from);
        let mut s = String::from("stack traceback:");
        // The main thread ends in the host's call (`[C]: in ?`), unless its
        // outermost level is already a native the host called (as `lua`'s
        // own driver is); a coroutine ends in its body, and a finished one
        // has no frames.
        let host = self
            .stack
            .frames
            .first()
            .is_some_and(|f| !matches!(f.kind, FrameKind::CoBase))
            && !levels
                .last()
                .is_some_and(|&l| func_info(heap, self, self.level_func(heap, l)).what == "C");
        let total = levels.len() + host as usize;
        let skip = if total > TRACE_FULL {
            Some((TRACE_HEAD, total - TRACE_HEAD - TRACE_TAIL))
        } else {
            None
        };
        for (i, &l) in levels.iter().enumerate() {
            if let Some((head, skipped)) = skip {
                if i == head {
                    // The reference prints one less than it skips.
                    let _ = write!(s, "\n\t...\t(skipping {} levels)", skipped - 1);
                }
                if i >= head && i < head + skipped {
                    continue;
                }
            }
            s.push_str("\n\t");
            self.describe_level(heap, l, &mut s);
            if self.is_tail_call(l) {
                s.push_str("\n\t(...tail calls...)");
            }
        }
        if host {
            s.push_str("\n\t[C]: in ?");
        }
        s
    }

    /// One traceback line, without the leading `\n\t`.
    fn describe_level(&self, heap: &LuaHeap, l: Level, s: &mut String) {
        let func = self.level_func(heap, l);
        let info = func_info(heap, self, func);
        if info.what == "C" {
            s.push_str("[C]: in ");
        } else {
            let line = self.current_line(heap, l);
            if line <= 0 {
                let _ = write!(s, "{}: in ", info.short_src);
            } else {
                let _ = write!(s, "{}:{}: in ", info.short_src, line);
            }
        }
        if let Some(name) = self.call_name(heap, l) {
            let _ = write!(
                s,
                "{} '{}'",
                name.namewhat,
                String::from_utf8_lossy(&name.name)
            );
        } else if info.what == "main" {
            s.push_str("main chunk");
        } else if let Some(g) = self.global_name(heap, func) {
            let _ = write!(s, "function '{}'", String::from_utf8_lossy(&g));
        } else if info.what == "C" {
            s.push('?');
        } else {
            let _ = write!(s, "function <{}:{}>", info.short_src, info.line_defined);
        }
    }

    /// The name a loaded module gives `f` (`string.rep`, or `print` for a
    /// global), searching `package.loaded` (the registry's `_LOADED`), or
    /// the globals when there is no such table.
    pub(crate) fn global_name(&self, heap: &LuaHeap, f: Value) -> Option<Vec<u8>> {
        let loaded = self.registry_field(heap, b"_LOADED");
        let search = |module: Gc<Table>, prefix: &[u8]| -> Option<Vec<u8>> {
            let t = heap.get(module);
            let mut k = Value::Nil;
            while let Ok(Some((key, v))) = t.next(heap, k) {
                if v.same(f)
                    && let Value::Str(s) = key
                {
                    let mut name = prefix.to_vec();
                    name.extend_from_slice(heap.get(s).as_bytes());
                    return Some(name);
                }
                k = key;
            }
            None
        };
        match loaded {
            Value::Table(loaded) => {
                let t = heap.get(loaded);
                let mut k = Value::Nil;
                while let Ok(Some((key, v))) = t.next(heap, k) {
                    if let (Value::Str(m), Value::Table(module)) = (key, v) {
                        let m = heap.get(m).as_bytes();
                        let prefix = if m == b"_G" {
                            Vec::new()
                        } else {
                            let mut p = m.to_vec();
                            p.push(b'.');
                            p
                        };
                        if let Some(name) = search(module, &prefix) {
                            return Some(name);
                        }
                    }
                    k = key;
                }
                None
            }
            _ => search(self.globals, b""),
        }
    }

    /// `registry[name]` (raw), for a string key.
    fn registry_field(&self, heap: &LuaHeap, name: &[u8]) -> Value {
        let reg = heap.get(self.registry);
        let mut k = Value::Nil;
        while let Ok(Some((key, v))) = reg.next(heap, k) {
            if let Value::Str(s) = key
                && heap.get(s).as_bytes() == name
            {
                return v;
            }
            k = key;
        }
        Value::Nil
    }
}

/// Whether a frame of this kind is a level of its own.
/// A meta frame below a `__close` handler that an unwinding error runs.
fn unwinding_close(k: FrameKind) -> bool {
    matches!(
        k,
        FrameKind::Meta {
            action: crate::vm::meta::MetaAction::Unwind { .. },
            ..
        }
    )
}

/// A frame an unwinding error stops at.
fn catches(k: FrameKind) -> bool {
    matches!(
        k,
        FrameKind::Native {
            catch: crate::vm::Catch::Yes | crate::vm::Catch::Handler,
            ..
        } | FrameKind::Boundary { .. }
            | FrameKind::CoBase
            | FrameKind::Hook
    )
}

fn is_activation(k: FrameKind) -> bool {
    match k {
        FrameKind::Lua { .. } | FrameKind::Native { .. } | FrameKind::NativeCall { .. } => true,
        FrameKind::Meta { .. }
        | FrameKind::Handler
        | FrameKind::Boundary { .. }
        | FrameKind::CoBase
        | FrameKind::Hook => false,
    }
}

/// Whether closure `c` belongs to a built-in library written in
/// OrbitScript, which the debug interface shows as a C function.
pub(crate) fn is_builtin(heap: &LuaHeap, c: Gc<Closure>) -> bool {
    heap.get(c).code.builtin
}

/// The function frame `i` runs.
fn frame_func(stack: &Stack, i: usize) -> Value {
    let f = &stack.frames[i];
    match f.kind {
        FrameKind::Lua { closure, .. } => Value::Closure(closure),
        FrameKind::Native { id, .. } | FrameKind::NativeCall { id } => Value::Native(id),
        FrameKind::Meta { .. }
        | FrameKind::Handler
        | FrameKind::Boundary { .. }
        | FrameKind::CoBase
        | FrameKind::Hook => Value::Nil,
    }
}

/// The line of instruction `pc`, or -1 without line information.
fn pc_line(proto: &Proto, pc: u32) -> i64 {
    if proto.debug.lines.is_empty() {
        -1
    } else {
        proto.debug.line(pc) as i64
    }
}

/// The named locals live at `pc`, in declaration order (parameters first),
/// with their slots. A vararg function's hidden vararg table follows its
/// parameters, as in the reference.
/// For the locals in scope at `pc`: how many there are, and the register
/// of the first temporary. Every local reserves a register from the bottom
/// of the window, even one kept in a cell, so the first temporary is past
/// both the highest named register and the count of locals with a slot
/// (the synthesized vararg table has none).
fn temporaries(proto: &Proto, pc: u32) -> (usize, usize) {
    let mut count = 0;
    let mut with_slot = 0;
    let mut top = proto.code.nparams as usize;
    for (_, slot) in active_locals(proto, pc) {
        count += 1;
        match slot {
            Some(LocalSlot::Reg(r)) => {
                with_slot += 1;
                top = top.max(r as usize + 1);
            }
            Some(LocalSlot::Cell(_)) => with_slot += 1,
            None => {}
        }
    }
    (count, top.max(with_slot))
}

impl Runtime {
    /// The `getlocal` index that reads register `reg` of Lua frame `fi`
    /// (for `getinfo`'s `ftransfer`).
    pub(crate) fn register_local(&self, heap: &LuaHeap, fi: usize, reg: usize) -> u32 {
        let f = &self.stack.frames[fi];
        let FrameKind::Lua { closure, .. } = f.kind else {
            return reg as u32 + 1;
        };
        let proto = heap.get(heap.get(closure).proto);
        let pc = f.pc.saturating_sub(1);
        let (count, first_free) = temporaries(proto, pc);
        if reg >= first_free {
            return (count + reg - first_free + 1) as u32;
        }
        active_locals(proto, pc)
            .position(|(_, s)| s == Some(LocalSlot::Reg(reg as u8)))
            .map_or(reg as u32 + 1, |i| i as u32 + 1)
    }
}

fn active_locals(
    proto: &Proto,
    pc: u32,
) -> impl Iterator<Item = (LocalName<'_>, Option<LocalSlot>)> {
    let np = proto.code.nparams as usize;
    let named = proto
        .debug
        .locals
        .iter()
        .filter(move |l| l.start_pc <= pc && pc < l.end_pc)
        .map(|l| (LocalName::Named(&l.name), Some(l.slot)));
    // The reference reports an unnamed `...` as a local after the
    // parameters; a named one (`...t`) is an ordinary local the compiler
    // declares at pc 0.
    let hidden_table = proto.code.is_vararg
        && !matches!(proto.name, orbit_bytecode::FunctionName::Main)
        && !proto.debug.lines.is_empty()
        && !proto
            .debug
            .locals
            .iter()
            .skip(np)
            .any(|l| l.start_pc == 0 && l.end_pc > pc);
    let params = named.clone().take(np);
    let rest = named.skip(np);
    params
        .chain(hidden_table.then_some((LocalName::VarargTable, None)))
        .chain(rest)
}

/// Static facts about function `f`.
pub(crate) fn func_info(heap: &LuaHeap, rt: &Runtime, f: Value) -> FuncInfo {
    match f {
        Value::Closure(c)
            if !matches!(
                heap.get(heap.get(c).proto).code.code.first(),
                Some(Op::NativeEntry { .. })
            ) && !is_builtin(heap, c) =>
        {
            let c = heap.get(c);
            let p = heap.get(c.proto);
            let is_main = matches!(p.name, orbit_bytecode::FunctionName::Main);
            FuncInfo {
                source: p.source.to_vec(),
                short_src: if p.source.as_ref() == b"=?" {
                    "?".into()
                } else {
                    p.chunk.to_string()
                },
                what: if is_main { "main" } else { "Lua" },
                line_defined: p.line_defined as i64,
                last_line: p.last_line as i64,
                nups: c.captures.len(),
                nparams: p.code.nparams,
                is_vararg: p.code.is_vararg,
            }
        }
        _ => FuncInfo {
            source: b"=[C]".to_vec(),
            short_src: "[C]".into(),
            what: "C",
            line_defined: -1,
            last_line: -1,
            // A native closure's upvalues, less the hidden ones.
            nups: match f {
                Value::Closure(c) => {
                    let c = heap.get(c);
                    c.captures.len() - rt.hidden_upvalues(heap, c)
                }
                _ => 0,
            },
            nparams: 0,
            is_vararg: true,
        },
    }
}

/// Lines with code in `f` (`getinfo`'s `activelines`).
pub(crate) fn active_lines(heap: &LuaHeap, f: Gc<Closure>) -> Vec<u32> {
    let p = heap.get(heap.get(f).proto);
    let mut lines: Vec<u32> = p.debug.lines.iter().map(|&(_, l)| l).collect();
    // The reference always ends a function with a return on its `end`
    // line, reachable or not; the compiler leaves out an unreachable one.
    if !lines.is_empty() && !matches!(p.name, orbit_bytecode::FunctionName::Main) {
        lines.push(p.last_line);
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// Called before a debug write to cell `c`. `CapSelf` loads the running
/// closure in place of its self capture, which is only right while that
/// capture's cell holds the closure; a debug write is the one thing that
/// can change it. So if `c` is some closure's self capture (the closure
/// is then `c`'s value), that closure gets code reading the capture.
pub(crate) fn before_cell_write(heap: &mut LuaHeap, c: Gc<Cell>) {
    if let Value::Closure(f) = heap.get(c).value {
        let closure = heap.get(f);
        if let Some(s) = heap.get(closure.proto).self_capture
            && closure.captures.get(s as usize) == Some(&c)
        {
            deopt_self(heap, f, s);
        }
    }
}

/// Give closure `f` code in which `CapSelf` reads capture `cap`.
pub(crate) fn deopt_self(heap: &mut LuaHeap, f: Gc<Closure>, cap: u8) {
    let code = &heap.get(f).code;
    if !code.code.iter().any(|op| matches!(op, Op::CapSelf { .. })) {
        return;
    }
    let new = code.without_cap_self(cap);
    heap.get_mut(f).code = std::rc::Rc::new(new);
}

/// The debug library's view of the VM.
impl crate::vm::NativeCtx<'_> {
    /// Set this stack's hook: `mask` is [`crate::vm::hook::CALL`] and friends,
    /// `count` the count interval (count events only when positive). A nil
    /// `f` or an empty mask with no count turns it off.
    pub fn set_hook(&mut self, f: Value, mask: u8, count: i64) {
        self.vm.set_hook(f, mask, count);
    }

    /// This stack's hook: function (nil when off), event mask without the
    /// count bit, and count.
    pub fn hook(&self) -> (Value, u8, i64) {
        let h = &self.vm.rt.stack.hook;
        use crate::vm::hook::{CALL, LINE, RETURN};
        (h.func, h.mask & (CALL | RETURN | LINE), h.base_count)
    }

    /// `getinfo`'s `ftransfer` and `ntransfer` for `l`: the values a call or
    /// return hook running for `l` transfers, else 0 and 0.
    pub fn transfer(&self, l: Level) -> (u32, u32) {
        match (l.0, self.vm.rt.stack.hook.transfer) {
            (LevelAt::Frame(i), Some((fi, first, n))) if i == fi => (first, n),
            _ => (0, 0),
        }
    }

    /// Activation `n` of a stack [`NativeCtx::on_thread`] put in place
    /// (`foreign`), where level 0 is its top activation, or of the running
    /// stack ([`NativeCtx::level`]).
    pub fn level_in(&self, foreign: bool, n: usize) -> Option<Level> {
        if foreign {
            self.vm.rt.level(None, n + 1)
        } else {
            self.level(n)
        }
    }

    /// [`NativeCtx::traceback`] for a stack `on_thread` put in place
    /// (`foreign`), whose levels start at 0 for its top activation.
    pub fn traceback_in(&self, foreign: bool, from: usize) -> String {
        if foreign {
            self.vm.rt.traceback(&self.vm.heap, None, from + 1)
        } else {
            self.traceback(from)
        }
    }

    /// Activation `n`: 0 is this native, 1 its caller, and so on.
    pub fn level(&self, n: usize) -> Option<Level> {
        self.vm.rt.level(Some(self.running_native()), n)
    }

    /// The function running at `l`.
    pub fn level_func(&self, l: Level) -> Value {
        self.vm.rt.level_func(&self.vm.heap, l)
    }

    /// The current line of `l` (-1 for natives).
    pub fn current_line(&self, l: Level) -> i64 {
        self.vm.rt.current_line(&self.vm.heap, l)
    }

    /// How `l` was called (`getinfo`'s `name` and `namewhat`).
    pub fn level_name(&self, l: Level) -> Option<CallName> {
        self.vm.rt.call_name(&self.vm.heap, l)
    }

    /// Whether `l` was entered by a tail call.
    pub fn is_tail_call(&self, l: Level) -> bool {
        self.vm.rt.is_tail_call(l)
    }

    /// Extra arguments a `__call` chain inserted for `l`.
    pub fn extra_args(&self, l: Level) -> u8 {
        self.vm.rt.extra_args(l)
    }

    /// Static facts about function `f` (a closure or native).
    pub fn func_info(&self, f: Value) -> FuncInfo {
        func_info(&self.vm.heap, &self.vm.rt, f)
    }

    /// Lines of `f` that have code, or `None` for a native.
    pub fn active_lines(&self, f: Value) -> Option<Vec<u32>> {
        match f {
            // A native closure is a C function to scripts: no lines.
            Value::Closure(c) if self.native_of(f).is_none() && !is_builtin(&self.vm.heap, c) => {
                Some(active_lines(&self.vm.heap, c))
            }
            _ => None,
        }
    }

    /// Name and value of local `n` of `l` (negative: varargs).
    pub fn get_local(&self, l: Level, n: i64) -> Option<(Vec<u8>, Value)> {
        let (name, place) = self.vm.rt.local_at(&self.vm.heap, l, n)?;
        Some((
            name.as_bytes().to_vec(),
            self.vm.rt.read_place(&self.vm.heap, place),
        ))
    }

    /// Set local `n` of `l`, returning its name.
    pub fn set_local(&mut self, l: Level, n: i64, v: Value) -> Option<Vec<u8>> {
        let (name, place) = self.vm.rt.local_at(&self.vm.heap, l, n)?;
        let name = name.as_bytes().to_vec();
        self.vm.rt.write_place(&mut self.vm.heap, place, v);
        Some(name)
    }

    /// Name of parameter `n` (1-based) of Lua function `f`.
    pub fn param_name(&self, f: Value, n: i64) -> Option<Vec<u8>> {
        let Value::Closure(c) = f else { return None };
        let p = self.vm.heap.get(self.vm.heap.get(c).proto);
        if n < 1 || n > p.code.nparams as i64 {
            return None;
        }
        active_locals(p, 0)
            .nth(n as usize - 1)
            .map(|(name, _)| name.as_bytes().to_vec())
    }

    /// The cell of upvalue `n` (1-based, in Lua's numbering) of `f`, with
    /// its name.
    pub fn upvalue_cell(&self, f: Value, n: i64) -> Option<(Vec<u8>, Gc<Cell>)> {
        let Value::Closure(c) = f else { return None };
        let c = self.vm.heap.get(c);
        let n = usize::try_from(n).ok()?.checked_sub(1)?;
        if n >= c.captures.len() - self.vm.rt.hidden_upvalues(&self.vm.heap, c) {
            return None;
        }
        let p = self.vm.heap.get(c.proto);
        let i = p.debug.capture_of_upvalue(n);
        let cell = c.captures[i];
        let name = match p.debug.capture_names.get(i) {
            // A native closure's upvalues have no names.
            _ if matches!(p.code.code.first(), Some(Op::NativeEntry { .. })) => Vec::new(),
            Some(name) if !p.debug.lines.is_empty() => name.to_vec(),
            _ => b"(no name)".to_vec(),
        };
        Some((name, cell))
    }

    /// Name and value of upvalue `n` of `f`.
    pub fn upvalue_of(&self, f: Value, n: i64) -> Option<(Vec<u8>, Value)> {
        let (name, cell) = self.upvalue_cell(f, n)?;
        Some((name, self.vm.heap.get(cell).value))
    }

    /// Set upvalue `n` of `f`, returning its name.
    pub fn set_upvalue_of(&mut self, f: Value, n: i64, v: Value) -> Option<Vec<u8>> {
        let (name, cell) = self.upvalue_cell(f, n)?;
        self.vm
            .rt
            .write_place(&mut self.vm.heap, Place::Cell(cell), v);
        Some(name)
    }

    /// Make upvalue `n1` of `f1` refer to upvalue `n2` of `f2`. Both must
    /// exist (the caller checks).
    pub fn join_upvalues(&mut self, f1: Gc<Closure>, n1: usize, f2: Gc<Closure>, n2: usize) {
        let p2 = self.vm.heap.get(f2).proto;
        let i2 = self.vm.heap.get(p2).debug.capture_of_upvalue(n2 - 1);
        let cell = self.vm.heap.get(f2).captures[i2];
        let proto = self.vm.heap.get(f1).proto;
        let i = self.vm.heap.get(proto).debug.capture_of_upvalue(n1 - 1);
        if self.vm.heap.get(proto).self_capture == Some(i as u8) {
            deopt_self(&mut self.vm.heap, f1, i as u8);
        }
        self.vm.heap.barrier_back(f1);
        self.vm.heap.get_mut(f1).captures[i] = cell;
    }

    /// `stack traceback:` and the levels from `from` down.
    pub fn traceback(&self, from: usize) -> String {
        self.vm
            .rt
            .traceback(&self.vm.heap, Some(self.running_native()), from)
    }
}
