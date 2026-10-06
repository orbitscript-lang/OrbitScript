//! The binary chunk format `string.dump` writes and `load` reads.
//!
//! ```text
//! "\x1bOrbit"   signature (its first byte is how `load` tells binary from text)
//! u8            format version, bumped whenever the encoding or the
//!               instruction set changes
//! u8            flags; bit 0: debug information stripped
//! ...           the chunk name, then each prototype in order
//! ```
//!
//! Unsigned integers are LEB128 varints, signed ones zig-zag varints, floats
//! their IEEE bits in 8 little-endian bytes, byte strings a length and the
//! bytes. Each [`Op`] is a tag byte and its operands. Nothing depends on the
//! host's word size or byte order.
//!
//! Decoding checks only that the bytes spell an [`UnlinkedChunk`]; whether
//! that chunk is safe to run is [`crate::verify`]'s job.

use core::fmt;

use crate::{
    CaptureSource, Constant, DebugInfo, FunctionName, LocalInfo, LocalSlot, Op, OperandOrigin,
    Origin, UnlinkedChunk, UnlinkedProto,
};

/// The first bytes of every binary chunk.
pub const SIGNATURE: &[u8] = b"\x1bOrbit";

/// The format version this build reads and writes.
pub const VERSION: u8 = 1;

const FLAG_STRIPPED: u8 = 1;

/// Why bytes don't decode as a chunk. The first three are the reference
/// interpreter's phrases, which `load` puts in `bad binary format (...)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The bytes don't start with [`SIGNATURE`].
    NotBinary,
    /// The bytes end before the chunk does.
    Truncated,
    /// Another format version.
    VersionMismatch,
    /// Something no encoder writes: an unknown tag, an out-of-range number,
    /// bytes after the end.
    Malformed(&'static str),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::NotBinary => f.write_str("not a binary chunk"),
            FormatError::Truncated => f.write_str("truncated chunk"),
            FormatError::VersionMismatch => f.write_str("version mismatch"),
            FormatError::Malformed(what) => write!(f, "malformed chunk: {what}"),
        }
    }
}

type Result<T> = core::result::Result<T, FormatError>;

/// Encode `chunk`. With `strip`, debug information is left out and the
/// chunk is named `?`, as the reference names a stripped chunk.
pub fn encode(chunk: &UnlinkedChunk, strip: bool) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.0.extend_from_slice(SIGNATURE);
    w.0.push(VERSION);
    w.0.push(if strip { FLAG_STRIPPED } else { 0 });
    w.bytes(if strip { b"?" } else { chunk.name.as_bytes() });
    w.uint(chunk.protos.len() as u64);
    for p in &chunk.protos {
        proto(&mut w, p, strip);
    }
    w.0
}

/// Decode a chunk [`encode`] wrote.
pub fn decode(bytes: &[u8]) -> Result<UnlinkedChunk> {
    // Too short to hold the signature is a truncated chunk, as in the
    // reference; a different signature isn't one of ours.
    if bytes.len() < SIGNATURE.len() {
        return Err(FormatError::Truncated);
    }
    if !bytes.starts_with(SIGNATURE) {
        return Err(FormatError::NotBinary);
    }
    let mut r = Reader {
        bytes,
        at: SIGNATURE.len(),
    };
    if r.byte()? != VERSION {
        return Err(FormatError::VersionMismatch);
    }
    if r.byte()? & !FLAG_STRIPPED != 0 {
        return Err(FormatError::Malformed("unknown flags"));
    }
    let name = String::from_utf8(r.bytes()?.into_vec())
        .map_err(|_| FormatError::Malformed("chunk name"))?
        .into_boxed_str();
    let protos = r.list(read_proto)?;
    if r.at != bytes.len() {
        return Err(FormatError::Malformed("bytes after the chunk"));
    }
    Ok(UnlinkedChunk { name, protos })
}

struct Writer(Vec<u8>);

impl Writer {
    fn uint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(b);
                return;
            }
            self.0.push(b | 0x80);
        }
    }

    fn int(&mut self, v: i64) {
        self.uint(((v << 1) ^ (v >> 63)) as u64);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.uint(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.bytes.get(self.at).ok_or(FormatError::Truncated)?;
        self.at += 1;
        Ok(b)
    }

    fn uint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            let bits = u64::from(b & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(FormatError::Malformed("varint overflow"));
            }
            v |= bits << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(FormatError::Malformed("varint overflow"))
    }

    fn int(&mut self) -> Result<i64> {
        let u = self.uint()?;
        Ok((u >> 1) as i64 ^ -((u & 1) as i64))
    }

    /// An element count. Every element takes at least one byte, so a count
    /// past the end of the input is a truncated chunk; this also keeps a
    /// corrupt count from reserving a huge allocation.
    fn count(&mut self) -> Result<usize> {
        let n = self.uint()?;
        if n > (self.bytes.len() - self.at) as u64 {
            return Err(FormatError::Truncated);
        }
        Ok(n as usize)
    }

    fn bytes(&mut self) -> Result<Box<[u8]>> {
        let n = self.count()?;
        let b = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(b.into())
    }

    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let n = self.count()?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(item(self)?);
        }
        Ok(v)
    }
}

/// An operand type and its encoding.
trait Field: Sized {
    fn put(self, w: &mut Writer);
    fn get(r: &mut Reader<'_>) -> Result<Self>;
}

impl Field for u8 {
    fn put(self, w: &mut Writer) {
        w.0.push(self);
    }
    fn get(r: &mut Reader<'_>) -> Result<Self> {
        r.byte()
    }
}

impl Field for bool {
    fn put(self, w: &mut Writer) {
        w.0.push(self as u8);
    }
    fn get(r: &mut Reader<'_>) -> Result<Self> {
        match r.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(FormatError::Malformed("boolean")),
        }
    }
}

macro_rules! unsigned_field {
    ($($t:ty),*) => {$(
        impl Field for $t {
            fn put(self, w: &mut Writer) {
                w.uint(self.into());
            }
            fn get(r: &mut Reader<'_>) -> Result<Self> {
                <$t>::try_from(r.uint()?).map_err(|_| FormatError::Malformed("operand out of range"))
            }
        }
    )*};
}

macro_rules! signed_field {
    ($($t:ty),*) => {$(
        impl Field for $t {
            fn put(self, w: &mut Writer) {
                w.int(self.into());
            }
            fn get(r: &mut Reader<'_>) -> Result<Self> {
                <$t>::try_from(r.int()?).map_err(|_| FormatError::Malformed("operand out of range"))
            }
        }
    )*};
}

unsigned_field!(u16, u32);
signed_field!(i8, i16, i32);

/// The instruction encoding: one tag per [`Op`] variant and its operands in
/// order. The matches it generates are exhaustive, so a new instruction
/// doesn't compile until it has a row here. Changing a row means bumping
/// [`VERSION`].
macro_rules! op_table {
    ($($tag:literal $name:ident $({ $($f:ident),* })?;)*) => {
        fn put_op(w: &mut Writer, op: Op) {
            match op {
                $(Op::$name $({ $($f),* })? => {
                    w.0.push($tag);
                    $($(Field::put($f, w);)*)?
                })*
            }
        }

        fn get_op(r: &mut Reader<'_>) -> Result<Op> {
            Ok(match r.byte()? {
                $($tag => Op::$name $({ $($f: Field::get(r)?),* })?,)*
                _ => return Err(FormatError::Malformed("unknown instruction")),
            })
        }
    };
}

op_table! {
    0 Move { dst, src };
    1 LoadNil { dst, count };
    2 LoadBool { dst, value };
    3 LoadInt { dst, value };
    4 LoadConst { dst, k };
    5 CellNew { cell, src };
    6 CellGet { dst, cell };
    7 CellSet { cell, src };
    8 CapGet { dst, cap };
    9 CapSet { cap, src };
    10 CapSelf { dst };
    11 GetGlobal { dst, name, ic };
    12 SetGlobal { src, name, ic };
    13 NewTable { dst, array_hint, hash_hint };
    14 GetField { dst, obj, key, ic };
    15 SetField { obj, key, src, ic };
    16 GetIndex { dst, obj, key };
    17 SetIndex { obj, key, src };
    18 GetIndexInt { dst, obj, key };
    19 SetIndexInt { obj, key, src };
    20 SetList { table, first, count, start };
    21 SelfCall { dst, obj, key };
    22 Add { dst, a, b };
    23 Sub { dst, a, b };
    24 Mul { dst, a, b };
    25 Div { dst, a, b };
    26 IDiv { dst, a, b };
    27 Mod { dst, a, b };
    28 Pow { dst, a, b };
    29 AddInt { dst, a, imm };
    30 Neg { dst, src };
    31 Not { dst, src };
    32 Len { dst, src };
    33 Concat { dst, first, count };
    34 Jump { off };
    35 JumpIfTrue { src, off };
    36 JumpIfFalse { src, off };
    37 JumpLt { a, b, off };
    38 JumpNotLt { a, b, off };
    39 JumpLe { a, b, off };
    40 JumpNotLe { a, b, off };
    41 JumpEq { a, b, off };
    42 JumpNe { a, b, off };
    43 JumpEqInt { a, imm, want, off };
    44 JumpLtInt { a, imm, want, off };
    45 JumpLeInt { a, imm, want, off };
    46 JumpGtInt { a, imm, want, off };
    47 JumpGeInt { a, imm, want, off };
    48 Call { func, nargs, nresults };
    49 TailCall { func, nargs };
    50 Return { first, count };
    51 Return0;
    52 Return1 { src };
    53 Vararg { dst, count };
    54 ForPrep { base, exit };
    55 ForLoop { base, back };
    56 GenericCall { base, nvars };
    57 GenericLoop { base, back };
    58 Closure { dst, proto };
}

/// One instruction's encoding (for tests and fuzzing).
pub fn encode_op(op: Op) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    put_op(&mut w, op);
    w.0
}

/// Decode one instruction from the start of `bytes` (for tests and fuzzing).
pub fn decode_op(bytes: &[u8]) -> Result<Op> {
    get_op(&mut Reader { bytes, at: 0 })
}

fn proto(w: &mut Writer, p: &UnlinkedProto, strip: bool) {
    match &p.name {
        FunctionName::Main => w.0.push(0),
        FunctionName::Anonymous => w.0.push(1),
        FunctionName::Global(n) => named(w, 2, n),
        FunctionName::Local(n) => named(w, 3, n),
        FunctionName::Field(n) => named(w, 4, n),
        FunctionName::Method(n) => named(w, 5, n),
    }
    w.uint(p.line_defined.into());
    w.uint(p.last_line.into());
    w.0.extend_from_slice(&[p.nparams, p.is_vararg as u8, p.nregs, p.ncells]);
    w.uint(p.ncaches.into());
    w.uint(p.code.len() as u64);
    for &op in &p.code {
        put_op(w, op);
    }
    w.uint(p.consts.len() as u64);
    for k in &p.consts {
        match k {
            Constant::Int(i) => {
                w.0.push(0);
                w.int(*i);
            }
            Constant::Float(f) => {
                w.0.push(1);
                w.0.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Constant::Str(s) => named(w, 2, s),
        }
    }
    w.uint(p.children.len() as u64);
    for &c in &p.children {
        w.uint(c.into());
    }
    w.uint(p.captures.len() as u64);
    for c in &p.captures {
        match *c {
            CaptureSource::Cell(i) => w.0.extend_from_slice(&[0, i]),
            CaptureSource::Capture(i) => w.0.extend_from_slice(&[1, i]),
            CaptureSource::Env => w.0.push(2),
        }
    }
    let empty = DebugInfo::default();
    let d = if strip { &empty } else { &p.debug };
    w.uint(d.lines.len() as u64);
    for &(pc, line) in &d.lines {
        w.uint(pc.into());
        w.uint(line.into());
    }
    w.uint(d.locals.len() as u64);
    for l in &d.locals {
        w.bytes(&l.name);
        match l.slot {
            LocalSlot::Reg(r) => w.0.extend_from_slice(&[0, r]),
            LocalSlot::Cell(c) => w.0.extend_from_slice(&[1, c]),
        }
        w.uint(l.start_pc.into());
        w.uint(l.end_pc.into());
    }
    w.uint(d.capture_names.len() as u64);
    for n in &d.capture_names {
        w.bytes(n);
    }
    w.uint(d.origins.len() as u64);
    for o in &d.origins {
        w.uint(o.pc.into());
        w.0.push(o.operand);
        match &o.origin {
            Origin::Local(n) => named(w, 0, n),
            Origin::Global(n) => named(w, 1, n),
            Origin::Field(n) => named(w, 2, n),
            Origin::Upvalue(n) => named(w, 3, n),
            Origin::Method(n) => named(w, 4, n),
            Origin::Constant(n) => named(w, 5, n),
        }
    }
}

fn named(w: &mut Writer, tag: u8, bytes: &[u8]) {
    w.0.push(tag);
    w.bytes(bytes);
}

fn read_proto(r: &mut Reader<'_>) -> Result<UnlinkedProto> {
    let name = match r.byte()? {
        0 => FunctionName::Main,
        1 => FunctionName::Anonymous,
        2 => FunctionName::Global(r.bytes()?),
        3 => FunctionName::Local(r.bytes()?),
        4 => FunctionName::Field(r.bytes()?),
        5 => FunctionName::Method(r.bytes()?),
        _ => return Err(FormatError::Malformed("function name")),
    };
    let line_defined = u32::get(r)?;
    let last_line = u32::get(r)?;
    let nparams = r.byte()?;
    let is_vararg = bool::get(r)?;
    let nregs = r.byte()?;
    let ncells = r.byte()?;
    let ncaches = u16::get(r)?;
    let code = r.list(get_op)?;
    let consts = r.list(|r| {
        Ok(match r.byte()? {
            0 => Constant::Int(r.int()?),
            1 => {
                let mut b = [0; 8];
                for x in &mut b {
                    *x = r.byte()?;
                }
                Constant::Float(f64::from_bits(u64::from_le_bytes(b)))
            }
            2 => Constant::Str(r.bytes()?),
            _ => return Err(FormatError::Malformed("constant")),
        })
    })?;
    let children = r.list(u32::get)?;
    let captures = r.list(|r| {
        Ok(match r.byte()? {
            0 => CaptureSource::Cell(r.byte()?),
            1 => CaptureSource::Capture(r.byte()?),
            2 => CaptureSource::Env,
            _ => return Err(FormatError::Malformed("capture")),
        })
    })?;
    let lines = r.list(|r| Ok((u32::get(r)?, u32::get(r)?)))?;
    let locals = r.list(|r| {
        let name = r.bytes()?;
        let slot = match r.byte()? {
            0 => LocalSlot::Reg(r.byte()?),
            1 => LocalSlot::Cell(r.byte()?),
            _ => return Err(FormatError::Malformed("local slot")),
        };
        Ok(LocalInfo {
            name,
            slot,
            start_pc: u32::get(r)?,
            end_pc: u32::get(r)?,
        })
    })?;
    let capture_names = r.list(Reader::bytes)?;
    let origins = r.list(|r| {
        let pc = u32::get(r)?;
        let operand = r.byte()?;
        let tag = r.byte()?;
        let n = r.bytes()?;
        let origin = match tag {
            0 => Origin::Local(n),
            1 => Origin::Global(n),
            2 => Origin::Field(n),
            3 => Origin::Upvalue(n),
            4 => Origin::Method(n),
            5 => Origin::Constant(n),
            _ => return Err(FormatError::Malformed("operand origin")),
        };
        Ok(OperandOrigin {
            pc,
            operand,
            origin,
        })
    })?;
    Ok(UnlinkedProto {
        name,
        line_defined,
        last_line,
        nparams,
        is_vararg,
        nregs,
        ncells,
        ncaches,
        code,
        consts,
        children,
        captures,
        debug: DebugInfo {
            lines,
            locals,
            capture_names,
            origins,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UnlinkedChunk {
        let s = |x: &str| -> Box<[u8]> { x.as_bytes().into() };
        let main = UnlinkedProto {
            name: FunctionName::Main,
            line_defined: 0,
            last_line: 3,
            nparams: 0,
            is_vararg: true,
            nregs: 3,
            ncells: 1,
            ncaches: 1,
            code: vec![
                Op::LoadInt {
                    dst: 0,
                    value: -70_000,
                },
                Op::CellNew { cell: 0, src: 0 },
                Op::Closure { dst: 1, proto: 0 },
                Op::GetGlobal {
                    dst: 2,
                    name: 0,
                    ic: 0,
                },
                Op::JumpLtInt {
                    a: 0,
                    imm: -3,
                    want: true,
                    off: -2,
                },
                Op::Return0,
            ],
            consts: vec![
                Constant::Str(s("print")),
                Constant::Float(-0.0),
                Constant::Int(i64::MIN),
            ],
            children: vec![1],
            captures: vec![CaptureSource::Env],
            debug: DebugInfo {
                lines: vec![(0, 1), (3, 2)],
                locals: vec![LocalInfo {
                    name: s("x"),
                    slot: LocalSlot::Cell(0),
                    start_pc: 1,
                    end_pc: 6,
                }],
                capture_names: vec![s("_ENV")],
                origins: vec![OperandOrigin {
                    pc: 3,
                    operand: 0,
                    origin: Origin::Global(s("print")),
                }],
            },
        };
        let child = UnlinkedProto {
            name: FunctionName::Local(s("f")),
            line_defined: 2,
            last_line: 2,
            nparams: 1,
            is_vararg: false,
            nregs: 1,
            ncells: 0,
            ncaches: 0,
            code: vec![Op::CapGet { dst: 0, cap: 0 }, Op::Return1 { src: 0 }],
            consts: vec![],
            children: vec![],
            captures: vec![CaptureSource::Cell(0)],
            debug: DebugInfo::default(),
        };
        UnlinkedChunk {
            name: "sample.lua".into(),
            protos: vec![main, child],
        }
    }

    #[test]
    fn round_trip() {
        let c = sample();
        let bytes = encode(&c, false);
        assert!(bytes.starts_with(SIGNATURE));
        let back = decode(&bytes).unwrap();
        // -0.0 == 0.0 under PartialEq, so compare float bits separately.
        assert_eq!(back, c);
        let Constant::Float(f) = back.protos[0].consts[1] else {
            panic!()
        };
        assert!(f.is_sign_negative());
    }

    #[test]
    fn strip_drops_debug_information() {
        let back = decode(&encode(&sample(), true)).unwrap();
        assert_eq!(&*back.name, "?");
        assert!(back.protos.iter().all(|p| p.debug == DebugInfo::default()));
        assert_eq!(back.protos[0].code, sample().protos[0].code);
    }

    #[test]
    fn errors() {
        let bytes = encode(&sample(), false);
        assert_eq!(decode(b""), Err(FormatError::Truncated));
        assert_eq!(decode(b"\x1bOr"), Err(FormatError::Truncated));
        assert_eq!(decode(b"\x1bLua"), Err(FormatError::Truncated));
        assert_eq!(decode(b"\x1bLua\x55\0"), Err(FormatError::NotBinary));
        assert_eq!(decode(b"return 1"), Err(FormatError::NotBinary));
        let mut v = bytes.clone();
        v[SIGNATURE.len()] = VERSION + 1;
        assert_eq!(decode(&v), Err(FormatError::VersionMismatch));
        for n in SIGNATURE.len()..bytes.len() {
            assert_eq!(decode(&bytes[..n]), Err(FormatError::Truncated), "{n}");
        }
        let mut v = bytes.clone();
        v.push(0);
        assert!(matches!(decode(&v), Err(FormatError::Malformed(_))));
    }

    #[test]
    fn huge_counts_are_truncation() {
        let mut w = Writer(Vec::new());
        w.0.extend_from_slice(SIGNATURE);
        w.0.extend_from_slice(&[VERSION, 0]);
        w.bytes(b"x");
        w.uint(u64::MAX);
        assert_eq!(decode(&w.0), Err(FormatError::Truncated));
    }
}
