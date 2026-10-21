//! Linking: turning the compiler's plain-data output into heap objects.

use std::collections::HashMap;
use std::rc::Rc;

use crate::object::string::MAX_SHORT;
use crate::object::{Cell, Closure, FieldCache, Gc, LuaHeap, LuaStr, Proto, ProtoCode, Value};
use orbit_bytecode::{CaptureSource, Constant, Op, UnlinkedChunk};

use crate::vm::{Runtime, Vm};

impl Vm {
    /// Link a compiled chunk and return its main function as a closure whose
    /// `_ENV` is the global table. The result is unrooted: call it (or pin
    /// it) before running anything else.
    pub fn load(&mut self, chunk: &UnlinkedChunk) -> Value {
        let env = Value::Table(self.rt.globals);
        link(&mut self.heap, &mut self.rt, chunk, env, false)
    }

    /// [`Vm::load`] with `env` as the main function's first capture.
    pub fn load_with_env(&mut self, chunk: &UnlinkedChunk, env: Value) -> Value {
        link(&mut self.heap, &mut self.rt, chunk, env, false)
    }

    /// [`Vm::load_with_env`] for a chunk of a built-in library: its
    /// functions show as C functions to the debug interface, and as `[C]`
    /// in messages and tracebacks.
    pub(crate) fn load_builtin(&mut self, chunk: &UnlinkedChunk, env: Value) -> Value {
        link(&mut self.heap, &mut self.rt, chunk, env, true)
    }
}

/// Link `chunk` into the heap. As `load` does in Lua, the main function's
/// first capture gets a cell holding `env`, and so does any capture of the
/// chunk's `_ENV` ([`CaptureSource::Env`]); every other capture gets a fresh
/// cell holding nil. A compiled chunk's main function captures only `_ENV`;
/// a dumped function keeps one capture per upvalue it had.
pub(crate) fn link(
    heap: &mut LuaHeap,
    rt: &mut Runtime,
    chunk: &UnlinkedChunk,
    env: Value,
    builtin: bool,
) -> Value {
    {
        let name: Rc<str> = Rc::from(if builtin { "[C]" } else { &*chunk.name });
        let source: Rc<[u8]> = if builtin {
            Rc::from(b"=[C]".as_slice())
        } else if chunk.source.is_empty() {
            let mut s = b"@".to_vec();
            s.extend_from_slice(chunk.name.as_bytes());
            s.into()
        } else {
            Rc::from(&*chunk.source)
        };
        let n = chunk.protos.len();
        // Children come after their parents, so link from the end and every
        // child is ready when its parent is built. Nothing collects here:
        // allocation never runs the collector.
        let mut linked: Vec<Option<Gc<Proto>>> = vec![None; n];
        // Equal long-string constants share one string across the chunk's functions, as in
        // lua5.5 (`string.format("%p")` shows it). Short strings are interned anyway.
        let mut long_strs: HashMap<&[u8], Gc<LuaStr>> = HashMap::new();
        for i in (0..n).rev() {
            let u = &chunk.protos[i];
            let consts: Vec<Value> = u
                .consts
                .iter()
                .map(|k| match k {
                    Constant::Int(i) => Value::Int(*i),
                    Constant::Float(f) => Value::Float(*f),
                    Constant::Str(b) if b.len() <= MAX_SHORT => {
                        Value::Str(rt.strings.intern(heap, b))
                    }
                    Constant::Str(b) => Value::Str(
                        *long_strs
                            .entry(b)
                            .or_insert_with(|| rt.strings.new_str(heap, b)),
                    ),
                })
                .collect();
            // Each cache gets its instruction's key. Indexes saturate in a
            // function with more sites than fit an operand; a cache whose
            // sites disagree on the key is marked shared and never fills.
            let mut caches: Vec<Option<FieldCache>> = (0..u.ncaches).map(|_| None).collect();
            let index_key = rt.field_name(crate::vm::meta::MetaField::Index);
            let mut claim = |ic: u16, key| match &mut caches[ic as usize] {
                slot @ None => *slot = Some(FieldCache::new(heap, key)),
                Some(c) if c.key != key => c.shared = true,
                Some(_) => {}
            };
            for op in &u.code {
                let (Op::GetGlobal { name: k, ic, .. }
                | Op::SetGlobal { name: k, ic, .. }
                | Op::GetField { key: k, ic, .. }
                | Op::SetField { key: k, ic, .. }
                | Op::SelfCall { key: k, ic, .. }) = *op
                else {
                    continue;
                };
                let Value::Str(key) = consts[k as usize] else {
                    unreachable!("field keys are strings")
                };
                claim(ic, key);
                // A method site's second cache finds `__index` in the
                // object's metatable.
                if let Op::SelfCall { .. } = op {
                    claim(ic + 1, index_key);
                    // And one level up the `__index` chain.
                    if (ic as u32 + 3) < u.ncaches as u32 {
                        claim(ic + 2, index_key);
                        claim(ic + 3, key);
                    }
                }
            }
            // A cache no instruction uses never fills, so it never hits and
            // its key is never compared: any string will do. Without string
            // constants there are no field sites, so no cache is used.
            let unused_key = consts.iter().find_map(|k| match k {
                Value::Str(s) => Some(*s),
                _ => None,
            });
            let caches: Box<[FieldCache]> = match unused_key {
                Some(key) => caches
                    .into_iter()
                    .map(|c| {
                        c.unwrap_or(FieldCache {
                            shared: true,
                            ..FieldCache::new(heap, key)
                        })
                    })
                    .collect(),
                None => Box::new([]),
            };
            let children = u
                .children
                .iter()
                .map(|&c| linked[c as usize].expect("child prototype comes after its parent"))
                .collect();
            let proto = Proto {
                code: Rc::new(ProtoCode {
                    code: u.code.as_slice().into(),
                    consts: consts.into(),
                    caches,
                    #[cfg(feature = "type-feedback")]
                    feedback: (0..u.code.len()).map(|_| std::cell::Cell::new(0)).collect(),
                    nparams: u.nparams,
                    is_vararg: u.is_vararg,
                    nregs: u.nregs,
                    ncells: u.ncells,
                    builtin,
                    refuses_yield: builtin,
                    #[cfg(feature = "debugger")]
                    debug_mark: Default::default(),
                }),
                children,
                captures: u.captures.as_slice().into(),
                name: u.name.clone(),
                line_defined: u.line_defined,
                last_line: u.last_line,
                chunk: name.clone(),
                source: source.clone(),
                self_capture: u.self_capture,
                debug: Rc::new(u.debug.clone()),
            };
            linked[i] = Some(heap.alloc(proto));
        }
        #[cfg(feature = "debugger")]
        if rt.debug.watch && !builtin {
            let protos: Vec<Gc<Proto>> = linked.iter().flatten().copied().collect();
            rt.debug.linked(heap, &protos);
        }
        let main = linked[0].expect("chunk has a main function");
        let main_proto = heap.get(main);
        let code = main_proto.code.clone();
        let sources = main_proto.captures.clone();
        let env_cell = heap.alloc(Cell { value: env });
        let captures = sources
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                if i == 0 || c == CaptureSource::Env {
                    env_cell
                } else {
                    heap.alloc(Cell::default())
                }
            })
            .collect();
        Value::Closure(heap.alloc(Closure {
            proto: main,
            code,
            captures,
        }))
    }
}
