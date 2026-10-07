//! The bytecode the compiler emits and the VM runs ([VM design], ADR-4).
//!
//! Everything here is plain `Send` data with no GC handles: the compiler
//! produces an [`UnlinkedChunk`], and `orbit-vm` links it into a heap
//! (interning strings, allocating prototypes bottom-up). It is its own crate,
//! with no dependencies, because three crates need it: `orbit-compiler`
//! emits it, `orbit-core`'s `Proto` stores it, and `orbit-vm` runs it (and
//! re-exports it as `orbit_vm::op`). The compiler thereby stays free of the
//! runtime and the GC.
//!
//! Notation in the instruction docs: `R[x]` is register `x` of the current
//! frame, `K[x]` constant `x`, `C[x]` frame cell slot `x`, `U[x]` capture `x`
//! of the running closure. Jump offsets are relative to the instruction after
//! the jump. "Top" is the register just past the last value produced by the
//! most recent open-ended call or `Vararg` (one whose count is [`MULTI`]).
//!
//! [VM design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md

pub mod number;

/// A count operand meaning "all values": as `nresults`, keep every result
/// and set top; as `nargs` or a `count`, use every value up to top.
pub const MULTI: u8 = 255;

/// The most registers one function may use.
pub const MAX_REGS: usize = 250;

/// The most captures one function may have.
pub const MAX_CAPTURES: usize = 255;

/// Register index.
pub type Reg = u8;
/// Constant-pool index.
pub type Const = u16;
/// Inline-cache slot index (one per global or constant-key field lookup).
pub type Ic = u16;
/// Jump offset, relative to the next instruction.
pub type Offset = i32;

/// One instruction. Always 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // Loads.
    /// `R[dst] = R[src]`
    Move { dst: Reg, src: Reg },
    /// `R[dst..dst+count] = nil`
    LoadNil { dst: Reg, count: u8 },
    /// `R[dst] = value`
    LoadBool { dst: Reg, value: bool },
    /// `R[dst] = value` as an integer.
    LoadInt { dst: Reg, value: i32 },
    /// `R[dst] = K[k]`
    LoadConst { dst: Reg, k: Const },

    // Cells and captures.
    /// `C[cell] = new cell holding R[src]`
    CellNew { cell: u8, src: Reg },
    /// `R[dst] = C[cell].value`
    CellGet { dst: Reg, cell: u8 },
    /// `C[cell].value = R[src]`
    CellSet { cell: u8, src: Reg },
    /// `R[dst] = U[cap].value`
    CapGet { dst: Reg, cap: u8 },
    /// `U[cap].value = R[src]`
    CapSet { cap: u8, src: Reg },
    /// `R[dst] = ` the running closure. The compiler emits it instead of
    /// `CapGet` when a `local function` refers to itself and the local is
    /// never assigned, so the capture always holds this closure.
    CapSelf { dst: Reg },

    // Globals, through the `_ENV` table in `U[0]`.
    /// `R[dst] = U[0].value[K[name]]`
    GetGlobal { dst: Reg, name: Const, ic: Ic },
    /// `U[0].value[K[name]] = R[src]`
    SetGlobal { src: Reg, name: Const, ic: Ic },
    /// Raises "global 'K[name]' already defined" unless `R[src]` is nil (Lua's
    /// `global x = v`, before the store).
    CheckUndefined { src: Reg, name: Const },

    // Tables.
    /// `R[dst] = {}` with room for `array_hint` array and `hash_hint` hash entries.
    NewTable {
        dst: Reg,
        array_hint: u16,
        hash_hint: u16,
    },
    /// `R[dst] = R[obj][K[key]]` (`K[key]` is a string).
    GetField {
        dst: Reg,
        obj: Reg,
        key: Const,
        ic: Ic,
    },
    /// `R[obj][K[key]] = R[src]` (`K[key]` is a string).
    SetField {
        obj: Reg,
        key: Const,
        src: Reg,
        ic: Ic,
    },
    /// `R[dst] = R[obj][R[key]]`
    GetIndex { dst: Reg, obj: Reg, key: Reg },
    /// `R[obj][R[key]] = R[src]`
    SetIndex { obj: Reg, key: Reg, src: Reg },
    /// `R[dst] = R[obj][key]` for an integer constant key.
    GetIndexInt { dst: Reg, obj: Reg, key: u16 },
    /// `R[obj][key] = R[src]` for an integer constant key.
    SetIndexInt { obj: Reg, key: u16, src: Reg },
    /// `R[table][start + i] = R[first + i]` for `i` in `0..count`
    /// (`count` [`MULTI`]: up to top).
    SetList {
        table: Reg,
        first: Reg,
        count: u8,
        start: u32,
    },
    /// Method lookup: `R[dst+1] = R[obj]; R[dst] = R[obj][K[key]]`.
    SelfCall { dst: Reg, obj: Reg, key: Const },

    // Arithmetic. Integer and float fast paths; everything else (mixed
    // numbers, string coercion, errors) goes to the VM's slow path.
    /// `R[dst] = R[a] + R[b]`
    Add { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] - R[b]`
    Sub { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] * R[b]`
    Mul { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] / R[b]` (always a float)
    Div { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] // R[b]` (floor division)
    IDiv { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] % R[b]` (floored modulo)
    Mod { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] ^ R[b]` (always a float)
    Pow { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] + imm`. Error messages treat `imm` as a constant
    /// operand (operand 1), as if it were `R[b]`.
    AddInt { dst: Reg, a: Reg, imm: i16 },
    /// `R[dst] = R[a] & R[b]`. Integer fast path; floats with an integer
    /// value convert, everything else goes to the VM's slow path.
    BAnd { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] | R[b]`
    BOr { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] ~ R[b]` (exclusive or)
    BXor { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] << R[b]` (logical; counts of 64 or more give 0, negative
    /// counts shift right)
    Shl { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = R[a] >> R[b]` (logical, like `Shl` with the count negated)
    Shr { dst: Reg, a: Reg, b: Reg },
    /// `R[dst] = -R[src]`
    Neg { dst: Reg, src: Reg },
    /// `R[dst] = ~R[src]`
    BNot { dst: Reg, src: Reg },
    /// `R[dst] = not R[src]`
    Not { dst: Reg, src: Reg },
    /// `R[dst] = #R[src]`
    Len { dst: Reg, src: Reg },
    /// `R[dst] = R[first] .. ... .. R[first+count-1]`
    Concat { dst: Reg, first: Reg, count: u8 },

    // Branches.
    /// `pc += off`
    Jump { off: Offset },
    /// `if R[src] is truthy { pc += off }`
    JumpIfTrue { src: Reg, off: Offset },
    /// `if R[src] is falsy { pc += off }`
    JumpIfFalse { src: Reg, off: Offset },
    /// `if R[a] < R[b] { pc += off }`
    JumpLt { a: Reg, b: Reg, off: Offset },
    /// `if not (R[a] < R[b]) { pc += off }`
    JumpNotLt { a: Reg, b: Reg, off: Offset },
    /// `if R[a] <= R[b] { pc += off }`
    JumpLe { a: Reg, b: Reg, off: Offset },
    /// `if not (R[a] <= R[b]) { pc += off }`
    JumpNotLe { a: Reg, b: Reg, off: Offset },
    /// `if R[a] == R[b] { pc += off }`
    JumpEq { a: Reg, b: Reg, off: Offset },
    /// `if R[a] ~= R[b] { pc += off }`
    JumpNe { a: Reg, b: Reg, off: Offset },
    /// `if (R[a] == imm) == want { pc += off }`
    JumpEqInt {
        a: Reg,
        imm: i8,
        want: bool,
        off: Offset,
    },
    /// `if (R[a] < imm) == want { pc += off }`. Errors as for `R[a] < imm`.
    JumpLtInt {
        a: Reg,
        imm: i8,
        want: bool,
        off: Offset,
    },
    /// `if (R[a] <= imm) == want { pc += off }`. Errors as for `R[a] <= imm`.
    JumpLeInt {
        a: Reg,
        imm: i8,
        want: bool,
        off: Offset,
    },
    /// `if (R[a] > imm) == want { pc += off }`. Errors as for `imm < R[a]`.
    JumpGtInt {
        a: Reg,
        imm: i8,
        want: bool,
        off: Offset,
    },
    /// `if (R[a] >= imm) == want { pc += off }`. Errors as for `imm <= R[a]`.
    JumpGeInt {
        a: Reg,
        imm: i8,
        want: bool,
        off: Offset,
    },

    // Calls.
    /// Call `R[func]` with `R[func+1..func+1+nargs]` (`nargs` [`MULTI`]: up
    /// to top). Results go to `R[func..]`: exactly `nresults` of them, padded
    /// with nil, or all of them if `nresults` is [`MULTI`] (sets top).
    Call { func: Reg, nargs: u8, nresults: u8 },
    /// `return R[func](args...)` without growing the frame stack. `nargs`
    /// as in [`Op::Call`]. Must be followed by `Return { first: func, count:
    /// MULTI }`: when the callee is a native that itself calls Lua code, the
    /// VM makes an ordinary call and that `Return` returns its results.
    TailCall { func: Reg, nargs: u8 },
    /// Return `R[first..first+count]` (`count` [`MULTI`]: up to top).
    Return { first: Reg, count: u8 },
    /// Return no values.
    Return0,
    /// Return `R[src]`.
    Return1 { src: Reg },
    /// `R[dst..dst+count] = ...` padded with nil (`count` [`MULTI`]: all of
    /// them, sets top).
    Vararg { dst: Reg, count: u8 },
    /// `R[dst] = {n = count, ...}`, a new table of the extra arguments (a Lua 5.5 named
    /// vararg).
    VarargTable { dst: Reg },
    /// `R[dst]` = what the vararg table would hold at `R[key]`, without building it: the
    /// extra argument for an integer key `1..count`, the count for `"n"`, nil otherwise.
    VarargGet { dst: Reg, key: Reg },
    /// Like `Vararg`, but reads the vararg table `R[table]` raw: `R[table][1..n]` with
    /// `n = R[table].n`, which must be an integer in `0..2^30` (error: "vararg table has no
    /// proper 'n'").
    VarargUnpack { dst: Reg, table: Reg, count: u8 },

    // Loops.
    /// Numeric `for` setup. On entry `R[base]` is the initial value,
    /// `R[base+1]` the limit and `R[base+2]` the step. Checks them (errors:
    /// `'for' initial value must be a number` etc.), then either jumps by
    /// `exit` (loop runs zero times) or sets `R[base+3]`, the control
    /// variable, to the first value. `R[base..base+3]` then hold private
    /// loop state in a VM-defined format (integer loops keep an iteration
    /// count; float loops keep the limit).
    ForPrep { base: Reg, exit: Offset },
    /// Numeric `for` step: advances the loop state in `R[base..base+3]`; if
    /// another iteration runs, sets `R[base+3]` and jumps by `back`.
    ForLoop { base: Reg, back: Offset },
    /// Generic `for` call: `R[base]` is the iterator, `R[base+1]` the state,
    /// `R[base+2]` the control value and `R[base+3]` the closing value
    /// (ignored at first light). Copies the first three to `R[base+4..]` and
    /// calls `R[base+4](R[base+5], R[base+6])` with `nvars` results, which
    /// land in `R[base+4..base+4+nvars]`, the loop variables.
    GenericCall { base: Reg, nvars: u8 },
    /// Generic `for` test: if `R[base+4]` is not nil, `R[base+2] = R[base+4]`
    /// and jump by `back`.
    GenericLoop { base: Reg, back: Offset },

    // Functions.
    /// `R[dst] = closure of child prototype proto`, its captures built from
    /// the child's [`CaptureSource`] list.
    Closure { dst: Reg, proto: u16 },
    /// The whole body of a native closure's stub prototype (never emitted
    /// by a compiler): run native `id` with the frame's arguments, the
    /// closure's captures as its upvalues, in place of this frame.
    NativeEntry { id: u32 },
}

const _: () = assert!(core::mem::size_of::<Op>() == 8);

/// A constant-pool entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Int(i64),
    Float(f64),
    /// A string's bytes (interned when linked).
    Str(Box<[u8]>),
}

/// Where a new closure gets one of its captures from, in the frame that
/// executes [`Op::Closure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// Frame cell slot `C[i]` of the enclosing function.
    Cell(u8),
    /// Capture `U[i]` of the enclosing closure.
    Capture(u8),
    /// The chunk's `_ENV` cell. Only in the main function's list, which the
    /// loader fills when it creates the main closure.
    Env,
}

/// How messages and tracebacks name a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionName {
    Main,
    Global(Box<[u8]>),
    Local(Box<[u8]>),
    Field(Box<[u8]>),
    Method(Box<[u8]>),
    Anonymous,
}

/// Where an operand of a fallible instruction came from, for messages such
/// as `attempt to index a nil value (local 't')`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Local(Box<[u8]>),
    Global(Box<[u8]>),
    Field(Box<[u8]>),
    Upvalue(Box<[u8]>),
    Method(Box<[u8]>),
    Constant(Box<[u8]>),
}

/// An operand origin at one instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperandOrigin {
    pub pc: u32,
    /// Which operand: 0 is the first one the VM checks (the indexed object
    /// of `GetField`/`GetIndex`/`SetField`/`SetIndex`/`SelfCall`, the
    /// function of `Call`/`TailCall`/`GenericCall`, `a` of arithmetic and
    /// comparisons, `src` of unary ops), 1 is `b` of binary ops; for
    /// `Concat` it is the position in the concatenated range.
    pub operand: u8,
    pub origin: Origin,
}

/// Where a named local lives while it is in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSlot {
    Reg(Reg),
    Cell(u8),
}

/// A named local's scope, for tracebacks and later `debug.getlocal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInfo {
    pub name: Box<[u8]>,
    pub slot: LocalSlot,
    /// First pc where the local is live.
    pub start_pc: u32,
    /// First pc where it no longer is.
    pub end_pc: u32,
}

/// Debug information kept beside the code (ADR-4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugInfo {
    /// Run-length line table: `(first_pc, line)` pairs in pc order. The line
    /// of `pc` is the one of the last pair whose `first_pc <= pc`.
    pub lines: Vec<(u32, u32)>,
    pub locals: Vec<LocalInfo>,
    /// One name per capture, in capture order.
    pub capture_names: Vec<Box<[u8]>>,
    /// Sorted by `(pc, operand)`.
    pub origins: Vec<OperandOrigin>,
}

impl DebugInfo {
    /// The source line of instruction `pc`, or 0 if unknown.
    pub fn line(&self, pc: u32) -> u32 {
        let i = self.lines.partition_point(|&(first, _)| first <= pc);
        if i == 0 { 0 } else { self.lines[i - 1].1 }
    }

    /// The recorded origin of `operand` at `pc`, if any.
    pub fn origin(&self, pc: u32, operand: u8) -> Option<&Origin> {
        self.origins
            .binary_search_by(|o| (o.pc, o.operand).cmp(&(pc, operand)))
            .ok()
            .map(|i| &self.origins[i].origin)
    }
}

/// One compiled function, before linking.
#[derive(Debug, Clone, PartialEq)]
pub struct UnlinkedProto {
    pub name: FunctionName,
    pub line_defined: u32,
    pub last_line: u32,
    pub nparams: u8,
    pub is_vararg: bool,
    /// Registers the function uses (at most [`MAX_REGS`]).
    pub nregs: u8,
    /// Frame cell slots the function uses.
    pub ncells: u8,
    /// Inline-cache slots the `ic` operands index.
    pub ncaches: u16,
    pub code: Vec<Op>,
    pub consts: Vec<Constant>,
    /// Child prototypes, as indices into [`UnlinkedChunk::protos`]; the
    /// `proto` operand of [`Op::Closure`] indexes this list.
    pub children: Vec<u32>,
    pub captures: Vec<CaptureSource>,
    pub debug: DebugInfo,
}

/// A whole compiled chunk. `protos[0]` is the main function; each
/// prototype's children come after it.
#[derive(Debug, Clone, PartialEq)]
pub struct UnlinkedChunk {
    /// The chunk name used in messages (`script.lua` in `script.lua:3:`).
    pub name: Box<str>,
    pub protos: Vec<UnlinkedProto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_lookup() {
        let d = DebugInfo {
            lines: vec![(0, 1), (3, 4), (7, 2)],
            ..DebugInfo::default()
        };
        assert_eq!(d.line(0), 1);
        assert_eq!(d.line(2), 1);
        assert_eq!(d.line(3), 4);
        assert_eq!(d.line(100), 2);
        assert_eq!(DebugInfo::default().line(5), 0);
    }

    #[test]
    fn origin_lookup() {
        let name = |s: &str| s.as_bytes().into();
        let d = DebugInfo {
            origins: vec![
                OperandOrigin {
                    pc: 2,
                    operand: 0,
                    origin: Origin::Local(name("t")),
                },
                OperandOrigin {
                    pc: 2,
                    operand: 1,
                    origin: Origin::Global(name("x")),
                },
                OperandOrigin {
                    pc: 5,
                    operand: 0,
                    origin: Origin::Field(name("a")),
                },
            ],
            ..DebugInfo::default()
        };
        assert_eq!(d.origin(2, 1), Some(&Origin::Global(name("x"))));
        assert_eq!(d.origin(5, 0), Some(&Origin::Field(name("a"))));
        assert_eq!(d.origin(3, 0), None);
    }
}
