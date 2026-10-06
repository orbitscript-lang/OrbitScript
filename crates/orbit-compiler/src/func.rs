//! Per-function compiler state: emission, registers, cell slots, labels, constants and debug
//! info. Expressions and statements are in `expr.rs` and `stmt.rs`.

use std::collections::HashMap;

use orbit_bytecode::{
    Constant, DebugInfo, FunctionName, LocalInfo, LocalSlot, MAX_CAPTURES, MAX_REGS, Offset, Op,
    OperandOrigin, Origin, UnlinkedProto,
};
use orbit_hir::{CaptureFrom, CaptureId, Chunk, FuncId, Function, LocalId, Span, StmtKind, StrId};

use crate::CompileError;
use crate::consts::ConstVal;

pub(crate) type Result<T> = std::result::Result<T, CompileError>;

/// Where a local lives while it's in scope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Slot {
    /// Not declared yet (or out of scope).
    Unset,
    Reg(u8),
    Cell(u8),
    /// A `<const>` local with a compile-time value; it occupies nothing at run time.
    Const(ConstVal),
}

/// A jump target inside the function being compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Label(usize);

#[derive(Default)]
struct LabelState {
    pos: Option<usize>,
    pending: Vec<usize>,
}

#[derive(Hash, PartialEq, Eq)]
enum ConstKey {
    Int(i64),
    Float(u64),
    Str(Box<[u8]>),
}

/// What compiling one function produces.
pub(crate) struct Compiled {
    pub proto: UnlinkedProto,
    /// Child functions, in the order of the `proto` operands of their `Closure` instructions.
    pub children: Vec<FuncId>,
    /// The cell slot each captured local was given (for the children's capture sources).
    pub cell_of: Vec<Option<u8>>,
}

pub(crate) struct FuncState<'a> {
    pub chunk: &'a Chunk,
    pub id: FuncId,
    pub f: &'a Function,
    /// `<const>` values of every function's locals, for folding captured constants too.
    pub const_locals: &'a [Vec<Option<ConstVal>>],
    code: Vec<Op>,
    lines: Vec<(u32, u32)>,
    line: u32,
    consts: Vec<Constant>,
    const_map: HashMap<ConstKey, u16>,
    ncaches: u32,
    pub children: Vec<FuncId>,
    /// Each child's index in `children`.
    child_index: HashMap<FuncId, u16>,
    pub slots: Vec<Slot>,
    cell_of: Vec<Option<u8>>,
    /// Next free register.
    pub free: usize,
    /// Registers below this belong to live locals (or loop state); above are temporaries.
    pub active: usize,
    nregs: usize,
    free_cell: usize,
    ncells: usize,
    labels: Vec<LabelState>,
    /// The compiler label for each HIR `goto` label, made on first use.
    goto_labels: Vec<Option<Label>>,
    /// Break targets of the enclosing loops.
    pub loops: Vec<Label>,
    origins: Vec<OperandOrigin>,
    debug_locals: Vec<LocalInfo>,
    /// Locals declared in the open scopes, with their `debug_locals` index.
    /// Hidden loop state has no `LocalId`.
    scope: Vec<(Option<LocalId>, Option<usize>)>,
    /// The capture that always holds the running closure, if any (see [`self_capture`]).
    pub self_capture: Option<CaptureId>,
}

/// Saved allocation state at the start of a scope.
pub(crate) struct ScopeMark {
    scope: usize,
    free: usize,
    active: usize,
    free_cell: usize,
}

impl<'a> FuncState<'a> {
    /// `self_local` is the parent's local this function is bound to by a `local function`
    /// statement, if any.
    pub fn new(
        chunk: &'a Chunk,
        id: FuncId,
        const_locals: &'a [Vec<Option<ConstVal>>],
        self_local: Option<LocalId>,
    ) -> Self {
        let f = chunk.function(id);
        FuncState {
            chunk,
            id,
            f,
            const_locals,
            code: Vec::new(),
            lines: Vec::new(),
            line: chunk.source.line_of(f.span.start),
            consts: Vec::new(),
            const_map: HashMap::new(),
            ncaches: 0,
            children: Vec::new(),
            child_index: HashMap::new(),
            slots: vec![Slot::Unset; f.locals.len()],
            cell_of: vec![None; f.locals.len()],
            free: 0,
            active: 0,
            nregs: 0,
            free_cell: 0,
            ncells: 0,
            labels: Vec::new(),
            goto_labels: vec![None; f.labels.len()],
            loops: Vec::new(),
            origins: Vec::new(),
            debug_locals: Vec::new(),
            scope: Vec::new(),
            self_capture: self_local.and_then(|l| self_capture(chunk, id, l)),
        }
    }

    pub fn compile(mut self) -> Result<Compiled> {
        let f = self.f;
        if f.captures.len() > MAX_CAPTURES {
            return Err(self.error("too many upvalues"));
        }
        if f.params.len() > MAX_REGS {
            return Err(self.error("too many parameters"));
        }
        for &p in &f.params {
            let r = self.alloc()?;
            self.active = self.free;
            self.declare(p, r)?;
        }
        if let Some(t) = f.vararg_table
            && !f.vararg_table_elided
        {
            let r = self.alloc()?;
            self.emit(Op::VarargTable { dst: r });
            self.active = self.free;
            self.declare(t, r)?;
        }
        self.block_stmts(f.body)?;
        let body = f.block(f.body);
        let ends_in_return = body
            .stmts
            .last()
            .is_some_and(|&s| matches!(f.stmt(s).kind, StmtKind::Return(_)));
        if !ends_in_return {
            self.at_line(self.last_line());
            self.emit(Op::Return0);
        }
        self.close_scope_locals(0);
        debug_assert!(
            self.labels.iter().all(|l| l.pending.is_empty()),
            "unplaced label"
        );

        let mut origins = std::mem::take(&mut self.origins);
        origins.sort_by_key(|o| (o.pc, o.operand));
        origins.dedup_by_key(|o| (o.pc, o.operand));
        let names = &self.chunk.strings;
        let bytes = |s: StrId| -> Box<[u8]> { names.get(s).into() };
        let name = match f.name {
            orbit_hir::FunctionName::Main => FunctionName::Main,
            orbit_hir::FunctionName::Global(s) => FunctionName::Global(bytes(s)),
            orbit_hir::FunctionName::Local(s) => FunctionName::Local(bytes(s)),
            orbit_hir::FunctionName::Field(s) => FunctionName::Field(bytes(s)),
            orbit_hir::FunctionName::Method(s) => FunctionName::Method(bytes(s)),
            orbit_hir::FunctionName::Anonymous => FunctionName::Anonymous,
        };
        let is_main = f.parent.is_none();
        let proto = UnlinkedProto {
            name,
            line_defined: if is_main {
                0
            } else {
                self.chunk.source.line_of(f.span.start)
            },
            last_line: if is_main { 0 } else { self.last_line() },
            nparams: f.params.len() as u8,
            is_vararg: f.is_vararg,
            nregs: self.nregs as u8,
            ncells: self.ncells as u8,
            ncaches: self.ncaches.min(u16::MAX as u32) as u16,
            code: self.code,
            consts: self.consts,
            children: Vec::new(),
            captures: Vec::new(),
            self_capture: self.self_capture.map(|c| c.index() as u8),
            debug: DebugInfo {
                lines: self.lines,
                locals: self.debug_locals,
                capture_names: f.captures.iter().map(|c| bytes(c.name)).collect(),
                env_debug_index: f.env_debug_index,
                origins,
            },
        };
        Ok(Compiled {
            proto,
            children: self.children,
            cell_of: self.cell_of,
        })
    }

    fn last_line(&self) -> u32 {
        self.chunk
            .source
            .line_of(self.f.span.end.saturating_sub(1).max(self.f.span.start))
    }

    // ---- errors and lines ----

    pub fn error(&self, message: &str) -> CompileError {
        CompileError {
            chunk: self.chunk.source.name.clone(),
            line: self.line,
            message: message.into(),
        }
    }

    /// Instructions emitted from now on are attributed to the line of `span`.
    pub fn at(&mut self, span: Span) {
        self.line = self.chunk.source.line_of(span.start);
    }

    pub fn at_line(&mut self, line: u32) {
        self.line = line;
    }

    // ---- emission ----

    pub fn emit(&mut self, op: Op) -> usize {
        let pc = self.code.len();
        if self.lines.last().is_none_or(|&(_, line)| line != self.line) {
            self.lines.push((pc as u32, self.line));
        }
        self.code.push(op);
        pc
    }

    /// Records where operand `operand` of the instruction at `pc` came from.
    pub fn origin(&mut self, pc: usize, operand: u8, origin: Option<Origin>) {
        if let Some(origin) = origin {
            self.origins.push(OperandOrigin {
                pc: pc as u32,
                operand,
                origin,
            });
        }
    }

    // ---- labels and jumps ----

    pub fn label(&mut self) -> Label {
        self.labels.push(LabelState::default());
        Label(self.labels.len() - 1)
    }

    /// The jump target for HIR label `l`.
    pub fn goto_label(&mut self, l: orbit_hir::LabelId) -> Label {
        if let Some(label) = self.goto_labels[l.index()] {
            return label;
        }
        let label = self.label();
        self.goto_labels[l.index()] = Some(label);
        label
    }

    /// A label placed at the current position (a backward jump target).
    pub fn here(&mut self) -> Label {
        let l = self.label();
        self.place(l);
        l
    }

    pub fn place(&mut self, label: Label) {
        let pos = self.code.len();
        let state = &mut self.labels[label.0];
        debug_assert!(state.pos.is_none(), "label placed twice");
        state.pos = Some(pos);
        for pc in std::mem::take(&mut state.pending) {
            let off = pos as Offset - (pc as Offset + 1);
            set_offset(&mut self.code[pc], off);
        }
    }

    /// Emits a jump instruction (built with a placeholder offset) aimed at `label`.
    pub fn jump(&mut self, op: Op, label: Label) -> usize {
        let pc = self.emit(op);
        match self.labels[label.0].pos {
            Some(pos) => set_offset(&mut self.code[pc], pos as Offset - (pc as Offset + 1)),
            None => self.labels[label.0].pending.push(pc),
        }
        pc
    }

    // ---- registers ----

    pub fn alloc(&mut self) -> Result<u8> {
        let r = self.free;
        self.free += 1;
        if self.free > MAX_REGS {
            return Err(self.error("function or expression needs too many registers"));
        }
        self.nregs = self.nregs.max(self.free);
        Ok(r as u8)
    }

    /// Makes sure the frame has room for registers up to (not including) `top`, for
    /// instructions that write beyond the allocated registers.
    pub fn reserve_to(&mut self, top: usize) -> Result<()> {
        if top > MAX_REGS {
            return Err(self.error("function or expression needs too many registers"));
        }
        self.nregs = self.nregs.max(top);
        Ok(())
    }

    // ---- constants and caches ----

    pub fn constant(&mut self, c: Constant) -> Result<u16> {
        let key = match &c {
            Constant::Int(i) => ConstKey::Int(*i),
            Constant::Float(x) => ConstKey::Float(x.to_bits()),
            Constant::Str(s) => ConstKey::Str(s.clone()),
        };
        if let Some(&k) = self.const_map.get(&key) {
            return Ok(k);
        }
        let Ok(k) = u16::try_from(self.consts.len()) else {
            return Err(self.error("too many constants"));
        };
        self.consts.push(c);
        self.const_map.insert(key, k);
        Ok(k)
    }

    pub fn str_constant(&mut self, s: StrId) -> Result<u16> {
        let bytes = self.chunk.strings.get(s).into();
        self.constant(Constant::Str(bytes))
    }

    /// A fresh inline-cache slot. Past `u16::MAX` slots, the last one is shared (a cache miss
    /// is only slower, never wrong).
    pub fn cache(&mut self) -> u16 {
        let ic = self.ncaches.min(u16::MAX as u32 - 1) as u16;
        self.ncaches += 1;
        ic
    }

    pub fn child(&mut self, func: FuncId) -> Result<u16> {
        if let Some(&i) = self.child_index.get(&func) {
            return Ok(i);
        }
        let Ok(i) = u16::try_from(self.children.len()) else {
            return Err(self.error("too many functions"));
        };
        self.children.push(func);
        self.child_index.insert(func, i);
        Ok(i)
    }

    // ---- locals and scopes ----

    pub fn name_bytes(&self, s: StrId) -> Box<[u8]> {
        self.chunk.strings.get(s).into()
    }

    pub fn local_origin(&self, l: LocalId) -> Option<Origin> {
        self.f
            .local(l)
            .name
            .map(|n| Origin::Local(self.name_bytes(n)))
    }

    pub fn open_scope(&self) -> ScopeMark {
        ScopeMark {
            scope: self.scope.len(),
            free: self.free,
            active: self.active,
            free_cell: self.free_cell,
        }
    }

    pub fn close_scope(&mut self, mark: ScopeMark) {
        self.close_scope_locals(mark.scope);
        self.free = mark.free;
        self.active = mark.active;
        self.free_cell = mark.free_cell;
    }

    fn close_scope_locals(&mut self, from: usize) {
        let end = self.code.len() as u32;
        for (l, debug) in self.scope.drain(from..) {
            if let Some(l) = l {
                self.slots[l.index()] = Slot::Unset;
            }
            if let Some(i) = debug {
                self.debug_locals[i].end_pc = end;
            }
        }
    }

    /// Brings local `l`, whose value is in register `r`, into scope. A captured local gets a
    /// fresh cell holding the value; a `<const>` with a compile-time value needs nothing.
    pub fn declare(&mut self, l: LocalId, r: u8) -> Result<()> {
        let local = self.f.local(l);
        let slot = if let Some(v) =
            self.const_locals[self.id.index()][l.index()].filter(|_| !local.captured)
        {
            Slot::Const(v)
        } else if local.captured {
            let cell = self.free_cell;
            self.free_cell += 1;
            if self.free_cell > u8::MAX as usize + 1 {
                return Err(self.error("too many captured local variables"));
            }
            self.ncells = self.ncells.max(self.free_cell);
            let cell = cell as u8;
            self.cell_of[l.index()] = Some(cell);
            self.emit(Op::CellNew { cell, src: r });
            Slot::Cell(cell)
        } else {
            Slot::Reg(r)
        };
        self.slots[l.index()] = slot;
        self.push_scope(l, slot);
        Ok(())
    }

    /// Records `l` as declared in the current scope with `slot` (for scope exit and the
    /// debug info's local list).
    pub fn push_scope(&mut self, l: LocalId, slot: Slot) {
        let local_slot = match slot {
            Slot::Reg(r) => Some(LocalSlot::Reg(r)),
            Slot::Cell(c) => Some(LocalSlot::Cell(c)),
            Slot::Const(_) | Slot::Unset => None,
        };
        let debug = match (self.f.local(l).name, local_slot) {
            (Some(name), Some(slot)) => {
                self.debug_locals.push(LocalInfo {
                    name: self.chunk.strings.get(name).into(),
                    slot,
                    start_pc: self.code.len() as u32,
                    end_pc: 0,
                });
                Some(self.debug_locals.len() - 1)
            }
            _ => None,
        };
        self.scope.push((Some(l), debug));
    }

    /// Records register `r` as hidden loop state, which `debug.getlocal` reports as
    /// `(for state)`, as the reference does.
    pub fn push_loop_state(&mut self, r: u8) {
        self.debug_locals.push(LocalInfo {
            name: b"(for state)".as_slice().into(),
            slot: LocalSlot::Reg(r),
            start_pc: self.code.len() as u32,
            end_pc: 0,
        });
        self.scope.push((None, Some(self.debug_locals.len() - 1)));
    }

    /// The compile-time value of capture `c` of this function, if it resolves to a `<const>`
    /// local of an enclosing function with a constant value.
    pub fn capture_const(&self, c: orbit_hir::CaptureId) -> Option<ConstVal> {
        let mut func = self.id;
        let mut cap = c;
        loop {
            let f = self.chunk.function(func);
            let parent = f.parent?;
            match f.capture(cap).from {
                orbit_hir::CaptureFrom::ParentLocal(l) => {
                    return self.const_locals[parent.index()][l.index()];
                }
                orbit_hir::CaptureFrom::ParentCapture(pc) => {
                    func = parent;
                    cap = pc;
                }
                orbit_hir::CaptureFrom::Env => return None,
            }
        }
    }
}

fn set_offset(op: &mut Op, new: Offset) {
    match op {
        Op::Jump { off }
        | Op::JumpIfTrue { off, .. }
        | Op::JumpIfFalse { off, .. }
        | Op::JumpLt { off, .. }
        | Op::JumpNotLt { off, .. }
        | Op::JumpLe { off, .. }
        | Op::JumpNotLe { off, .. }
        | Op::JumpEq { off, .. }
        | Op::JumpNe { off, .. }
        | Op::JumpEqInt { off, .. }
        | Op::JumpLtInt { off, .. }
        | Op::JumpLeInt { off, .. }
        | Op::JumpGtInt { off, .. }
        | Op::JumpGeInt { off, .. } => *off = new,
        Op::ForPrep { exit, .. } => *exit = new,
        Op::ForLoop { back, .. } | Op::GenericLoop { back, .. } => *back = new,
        _ => unreachable!("not a jump: {op:?}"),
    }
}

/// The capture of `id` that always holds `id`'s own closure: the one for `local`, which a
/// parent's `local function` statement binds `id` to, when nothing ever assigns that local.
/// The statement stores the closure in the local's cell before anything can call it, so reading
/// the capture is the same as reading the running closure, and `CapSelf` does that without
/// going through the closure's captures and the cell.
fn self_capture(chunk: &Chunk, id: FuncId, local: LocalId) -> Option<CaptureId> {
    let parent = chunk.function(chunk.function(id).parent?);
    if parent.locals[local.index()].reassigned {
        return None;
    }
    let f = chunk.function(id);
    let i = f
        .captures
        .iter()
        .position(|c| c.from == CaptureFrom::ParentLocal(local))?;
    Some(CaptureId::from_index(i))
}
