//! Binary chunks: `string.dump` and loading its output (`ldump.c`, `lundump.c`).
//!
//! The header is the reference's, so tools that inspect it (and the official tests)
//! see a Lua 5.5 chunk. The body is OrbitScript's own encoding of a prototype tree:
//! variable-length integers, strings stored once and referenced afterwards, and a
//! function's source omitted when it equals its parent's. Binary chunks from the
//! reference interpreter are not compatible and are rejected or fail to decode.
//!
//! Loading validates the encoding, not the bytecode: a hand-made chunk with bad
//! register or constant operands makes the interpreter panic rather than misbehave.

use crate::error::{Error, LuaResult, Status};
use crate::state::Vm;
use orbit_bytecode::{FnInfo, LocVar, UpvalDesc};
use orbit_core::Value;
use orbit_core::object::{LuaClosure, Proto, ProtoRef, UpVal};
use orbit_core::value::StrRef;
use std::collections::HashMap;
use std::rc::Rc;

const SIGNATURE: &[u8] = b"\x1bLua";
const VERSION: u8 = 0x55;
const FORMAT: u8 = 0;
const DATA: &[u8] = b"\x19\x93\r\n\x1a\n";
const CHECK_INT: i64 = -0x5678;
const CHECK_INST: u32 = 0x1234_5678;
const CHECK_NUM: f64 = -370.5;

// Constant tags.
const K_NIL: u8 = 0;
const K_FALSE: u8 = 1;
const K_TRUE: u8 = 2;
const K_INT: u8 = 3;
const K_FLOAT: u8 = 4;
const K_STR: u8 = 5;

struct Writer<'a> {
    vm: &'a Vm,
    out: Vec<u8>,
    strip: bool,
    /// Strings already written, by content, with their index.
    saved: HashMap<Vec<u8>, u64>,
}

impl Writer<'_> {
    fn varint(&mut self, mut n: u64) {
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                self.out.push(b);
                return;
            }
            self.out.push(b | 0x80);
        }
    }

    /// A string: 0 for none, 1 and the bytes for a new one, `2 + index` for one
    /// already written.
    fn string(&mut self, s: Option<&[u8]>) {
        let Some(s) = s else {
            self.varint(0);
            return;
        };
        if let Some(&i) = self.saved.get(s) {
            self.varint(2 + i);
            return;
        }
        let i = self.saved.len() as u64;
        self.saved.insert(s.to_vec(), i);
        self.varint(1);
        self.varint(s.len() as u64);
        self.out.extend_from_slice(s);
    }

    fn heap_str(&mut self, s: StrRef) {
        let bytes = self.vm.heap.string(s).as_bytes().to_vec();
        self.string(Some(&bytes));
    }

    fn function(&mut self, p: ProtoRef, parent_source: Option<StrRef>) {
        let vm = self.vm;
        let proto = vm.heap.get(p);
        let info = proto.info.clone();
        // The source, unless stripped or the same as the enclosing function's.
        let source = if self.strip || proto.source == parent_source {
            None
        } else {
            proto.source
        };
        match source {
            Some(s) => self.heap_str(s),
            None => self.string(None),
        }
        self.varint(u64::from(info.line_defined));
        self.varint(u64::from(info.last_line_defined));
        self.out
            .extend_from_slice(&[info.num_params, info.flags, info.max_stack]);
        self.varint(info.code.len() as u64);
        for &i in info.code.iter() {
            self.out.extend_from_slice(&i.to_le_bytes());
        }
        let constants = proto.constants.clone();
        self.varint(constants.len() as u64);
        for k in constants.iter() {
            match *k {
                Value::Nil => self.out.push(K_NIL),
                Value::Bool(false) => self.out.push(K_FALSE),
                Value::Bool(true) => self.out.push(K_TRUE),
                Value::Int(n) => {
                    self.out.push(K_INT);
                    self.out.extend_from_slice(&n.to_le_bytes());
                }
                Value::Float(x) => {
                    self.out.push(K_FLOAT);
                    self.out.extend_from_slice(&x.to_bits().to_le_bytes());
                }
                Value::Str(s) => {
                    self.out.push(K_STR);
                    self.heap_str(s);
                }
                _ => unreachable!("constant of unexpected type"),
            }
        }
        self.varint(info.upvalues.len() as u64);
        for u in info.upvalues.iter() {
            self.out
                .extend_from_slice(&[u8::from(u.in_stack), u.index, u.kind]);
        }
        let protos = proto.protos.clone();
        self.varint(protos.len() as u64);
        for &child in protos.iter() {
            self.function(child, proto.source);
        }
        // Debug information.
        if self.strip {
            self.varint(0);
            self.varint(0);
            self.varint(0);
            return;
        }
        self.varint(info.lines.len() as u64);
        let mut prev = i64::from(info.line_defined);
        for &l in info.lines.iter() {
            // Zigzag-encoded deltas keep line numbers to a byte each.
            let d = i64::from(l) - prev;
            prev = i64::from(l);
            self.varint(((d << 1) ^ (d >> 63)) as u64);
        }
        self.varint(info.locvars.len() as u64);
        for v in info.locvars.iter() {
            self.string(Some(v.name.as_bytes()));
            self.varint(u64::from(v.start_pc));
            self.varint(u64::from(v.end_pc));
        }
        self.varint(info.upvalues.len() as u64);
        for u in info.upvalues.iter() {
            let name = (!u.name.is_empty()).then_some(u.name.as_bytes());
            self.string(name);
        }
    }
}

/// A decoded function, before its constants and source move into the heap.
struct Decoded {
    source: Option<Vec<u8>>,
    info: FnInfo,
    constants: Vec<DecodedConst>,
    protos: Vec<Decoded>,
}

enum DecodedConst {
    Value(Value),
    Str(Vec<u8>),
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    saved: Vec<Vec<u8>>,
    depth: u32,
}

/// Why decoding stopped: the message inside "bad binary format (...)".
type Bad = &'static str;

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], Bad> {
        let end = self.pos.checked_add(n).ok_or("truncated chunk")?;
        let b = self.data.get(self.pos..end).ok_or("truncated chunk")?;
        self.pos = end;
        Ok(b)
    }

    fn byte(&mut self) -> Result<u8, Bad> {
        Ok(self.bytes(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Bad> {
        Ok(self.bytes(N)?.try_into().expect("N bytes"))
    }

    fn varint(&mut self) -> Result<u64, Bad> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            n |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
        }
        Err("integer overflow")
    }

    /// A count of items, each at least one byte, so a corrupt count cannot make
    /// the loader allocate much more than the chunk's size.
    fn count(&mut self) -> Result<usize, Bad> {
        let n = self.varint()?;
        if n > (self.data.len() - self.pos) as u64 {
            return Err("truncated chunk");
        }
        Ok(n as usize)
    }

    fn u32v(&mut self) -> Result<u32, Bad> {
        u32::try_from(self.varint()?).map_err(|_| "integer overflow")
    }

    fn string(&mut self) -> Result<Option<Vec<u8>>, Bad> {
        match self.varint()? {
            0 => Ok(None),
            1 => {
                let len = self.count()?;
                let s = self.bytes(len)?.to_vec();
                self.saved.push(s.clone());
                Ok(Some(s))
            }
            i => self
                .saved
                .get((i - 2) as usize)
                .cloned()
                .map(Some)
                .ok_or("invalid string reference"),
        }
    }

    fn name(&mut self) -> Result<Box<str>, Bad> {
        let s = self.string()?.unwrap_or_default();
        Ok(String::from_utf8_lossy(&s).into())
    }

    fn header(&mut self) -> Result<(), Bad> {
        if self.bytes(SIGNATURE.len())? != SIGNATURE {
            return Err("not a binary chunk");
        }
        if self.byte()? != VERSION {
            return Err("version mismatch");
        }
        if self.byte()? != FORMAT {
            return Err("format mismatch");
        }
        if self.bytes(DATA.len())? != DATA {
            return Err("corrupted chunk");
        }
        if self.byte()? != 4 || i32::from_ne_bytes(self.array()?) != CHECK_INT as i32 {
            return Err("int format mismatch");
        }
        if self.byte()? != 4 || u32::from_ne_bytes(self.array()?) != CHECK_INST {
            return Err("instruction format mismatch");
        }
        if self.byte()? != 8 || i64::from_ne_bytes(self.array()?) != CHECK_INT {
            return Err("Lua integer format mismatch");
        }
        if self.byte()? != 8 || f64::from_ne_bytes(self.array()?) != CHECK_NUM {
            return Err("Lua number format mismatch");
        }
        Ok(())
    }

    fn function(&mut self) -> Result<Decoded, Bad> {
        self.depth += 1;
        if self.depth > 200 {
            return Err("functions nested too deeply");
        }
        let source = self.string()?;
        let line_defined = self.u32v()?;
        let last_line_defined = self.u32v()?;
        let [num_params, flags, max_stack] = self.array()?;
        let ncode = self.count()?;
        let mut code = Vec::with_capacity(ncode);
        for _ in 0..ncode {
            code.push(u32::from_le_bytes(self.array()?));
        }
        if code.is_empty() {
            return Err("function without code");
        }
        let nk = self.count()?;
        let mut constants = Vec::with_capacity(nk);
        for _ in 0..nk {
            constants.push(match self.byte()? {
                K_NIL => DecodedConst::Value(Value::Nil),
                K_FALSE => DecodedConst::Value(Value::Bool(false)),
                K_TRUE => DecodedConst::Value(Value::Bool(true)),
                K_INT => DecodedConst::Value(Value::Int(i64::from_le_bytes(self.array()?))),
                K_FLOAT => DecodedConst::Value(Value::Float(f64::from_bits(u64::from_le_bytes(
                    self.array()?,
                )))),
                K_STR => DecodedConst::Str(self.string()?.ok_or("missing string constant")?),
                _ => return Err("invalid constant"),
            });
        }
        let nup = self.count()?;
        if nup > 255 {
            return Err("too many upvalues");
        }
        let mut upvalues = Vec::with_capacity(nup);
        for _ in 0..nup {
            let [in_stack, index, kind] = self.array()?;
            upvalues.push(UpvalDesc {
                name: "".into(),
                in_stack: in_stack != 0,
                index,
                kind,
            });
        }
        let np = self.count()?;
        let mut protos = Vec::with_capacity(np);
        for _ in 0..np {
            protos.push(self.function()?);
        }
        let nlines = self.count()?;
        if nlines != 0 && nlines != code.len() {
            return Err("bad line information");
        }
        let mut lines = Vec::with_capacity(nlines);
        let mut prev = i64::from(line_defined);
        for _ in 0..nlines {
            let z = self.varint()?;
            let d = (z >> 1) as i64 ^ -((z & 1) as i64);
            prev = prev.wrapping_add(d);
            lines.push(u32::try_from(prev).map_err(|_| "bad line information")?);
        }
        let nloc = self.count()?;
        let mut locvars = Vec::with_capacity(nloc);
        for _ in 0..nloc {
            let name = self.name()?;
            let start_pc = self.u32v()?;
            let end_pc = self.u32v()?;
            locvars.push(LocVar {
                name,
                start_pc,
                end_pc,
            });
        }
        let nupnames = self.count()?;
        if nupnames != 0 && nupnames != upvalues.len() {
            return Err("bad upvalue names");
        }
        for u in upvalues.iter_mut().take(nupnames) {
            u.name = self.name()?;
        }
        self.depth -= 1;
        Ok(Decoded {
            source,
            info: FnInfo {
                code: code.into(),
                lines: lines.into(),
                upvalues: upvalues.into(),
                locvars: locvars.into(),
                num_params,
                flags,
                max_stack,
                line_defined,
                last_line_defined,
            },
            constants,
            protos,
        })
    }
}

impl Vm {
    /// Serializes Lua function `f` as a binary chunk (`lua_dump`). `strip` leaves out
    /// debug information. Returns `None` for native functions.
    pub fn dump(&self, f: Value, strip: bool) -> Option<Vec<u8>> {
        let Value::LuaFn(cl) = f else { return None };
        let c = self.heap.get(cl);
        let mut w = Writer {
            vm: self,
            out: Vec::new(),
            strip,
            saved: HashMap::new(),
        };
        w.out.extend_from_slice(SIGNATURE);
        w.out.extend_from_slice(&[VERSION, FORMAT]);
        w.out.extend_from_slice(DATA);
        w.out.push(4);
        w.out.extend_from_slice(&(CHECK_INT as i32).to_ne_bytes());
        w.out.push(4);
        w.out.extend_from_slice(&CHECK_INST.to_ne_bytes());
        w.out.push(8);
        w.out.extend_from_slice(&CHECK_INT.to_ne_bytes());
        w.out.push(8);
        w.out.extend_from_slice(&CHECK_NUM.to_ne_bytes());
        w.out.push(c.upvals.len() as u8);
        w.function(c.proto, None);
        Some(w.out)
    }

    /// Loads a binary chunk made by [`Vm::dump`], with the global table as the value
    /// of its first upvalue (`luaU_undump`). Errors have [`Status::Syntax`].
    pub fn undump(&mut self, data: &[u8], chunkname: &[u8]) -> LuaResult<Value> {
        let mut r = Reader {
            data,
            pos: 0,
            saved: Vec::new(),
            depth: 0,
        };
        let decoded = r.header().and_then(|()| {
            let nup = r.byte()?;
            let f = r.function()?;
            if usize::from(nup) != f.info.upvalues.len() {
                return Err("upvalue count mismatch");
            }
            Ok(f)
        });
        let f = match decoded {
            Ok(f) => f,
            Err(why) => {
                let name: &[u8] = match chunkname.first() {
                    Some(b'@' | b'=') => &chunkname[1..],
                    Some(0x1b) => b"binary string",
                    _ => chunkname,
                };
                let mut msg = name.to_vec();
                msg.extend_from_slice(format!(": bad binary format ({why})").as_bytes());
                return Err(Error {
                    value: self.heap.str_value(msg),
                    status: Status::Syntax,
                });
            }
        };
        let proto = self.alloc_decoded(f, None, &mut HashMap::new());
        let nup = self.heap.get(proto).num_upvalues();
        let mut upvals = Vec::with_capacity(nup);
        for i in 0..nup {
            let v = if i == 0 {
                Value::Table(self.globals)
            } else {
                Value::Nil
            };
            upvals.push(self.heap.alloc(UpVal::Closed(v)));
        }
        let cl = self.heap.alloc(LuaClosure {
            proto,
            upvals: upvals.into_boxed_slice(),
        });
        Ok(Value::LuaFn(cl))
    }

    fn alloc_decoded(
        &mut self,
        f: Decoded,
        parent_source: Option<StrRef>,
        strings: &mut HashMap<Vec<u8>, StrRef>,
    ) -> ProtoRef {
        let source = match &f.source {
            Some(s) => Some(self.heap.intern(s)),
            None => parent_source,
        };
        let constants: Vec<Value> = f
            .constants
            .into_iter()
            .map(|k| match k {
                DecodedConst::Value(v) => v,
                DecodedConst::Str(s) => Value::Str(match strings.get(&s) {
                    Some(&r) => r,
                    None => {
                        let r = self.heap.intern(&s);
                        strings.insert(s, r);
                        r
                    }
                }),
            })
            .collect();
        let protos: Vec<ProtoRef> = f
            .protos
            .into_iter()
            .map(|p| self.alloc_decoded(p, source, strings))
            .collect();
        self.heap.alloc(Proto {
            constants: constants.into_boxed_slice(),
            protos: protos.into_boxed_slice(),
            source,
            info: Rc::new(f.info),
        })
    }
}
