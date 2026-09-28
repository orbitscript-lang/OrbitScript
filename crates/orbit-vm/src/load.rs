//! Loading source chunks: parse, compile, and move the result into the heap
//! (`lua_load`).

use crate::error::{Error, LuaResult, Status};
use crate::state::Vm;
use orbit_compiler::{CompiledFn, Constant};
use orbit_core::Value;
use orbit_core::object::ProtoRef;
use orbit_core::object::{LuaClosure, Proto, UpVal};
use orbit_core::value::StrRef;
use orbit_lua_syntax::ErrorKind;
use std::collections::HashMap;
use std::rc::Rc;

impl Vm {
    /// Compiles `src` as a chunk named `chunkname` (`=name`, `@file` or the source
    /// itself, as in the reference) and returns the main function, with its first
    /// upvalue (`_ENV`) set to the global table.
    ///
    /// A syntax error is returned as an [`Error`] with [`Status::Syntax`] whose value is
    /// the message `load` would return.
    pub fn load(&mut self, src: &[u8], chunkname: &[u8]) -> LuaResult<Value> {
        let env = Value::Table(self.globals);
        self.load_with_env(src, chunkname, Some(env))
    }

    /// Like [`Vm::load`], with `env` as the value of the first upvalue (`None` leaves
    /// it `nil`).
    pub fn load_with_env(
        &mut self,
        src: &[u8],
        chunkname: &[u8],
        env: Option<Value>,
    ) -> LuaResult<Value> {
        let compiled = orbit_lua_syntax::parse(src).and_then(|c| orbit_compiler::compile(&c));
        let f = match compiled {
            Ok(f) => f,
            Err(e) => {
                let msg = e.to_lua_message(chunkname);
                let status = if e.kind == ErrorKind::Overflow {
                    Status::Runtime
                } else {
                    Status::Syntax
                };
                return Err(Error {
                    value: self.heap.str_value(msg),
                    status,
                });
            }
        };
        let source = self.heap.intern(chunkname);
        let proto = self.make_proto(f, source, &mut HashMap::new());
        let nup = self.heap.get(proto).num_upvalues();
        let mut upvals = Vec::with_capacity(nup);
        for i in 0..nup {
            let v = if i == 0 {
                env.unwrap_or(Value::Nil)
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

    /// Moves a compiled function into the heap. Equal string constants of one chunk
    /// share a string object, long ones included, as the reference's lexer does.
    fn make_proto(
        &mut self,
        f: CompiledFn,
        source: StrRef,
        strings: &mut HashMap<Vec<u8>, StrRef>,
    ) -> ProtoRef {
        let constants: Vec<Value> = f
            .constants
            .iter()
            .map(|c| match c {
                Constant::Nil => Value::Nil,
                Constant::Bool(b) => Value::Bool(*b),
                Constant::Int(n) => Value::Int(*n),
                Constant::Float(x) => Value::Float(*x),
                Constant::Str(s) => Value::Str(match strings.get(&s[..]) {
                    Some(&r) => r,
                    None => {
                        let r = self.heap.intern(s);
                        strings.insert(s.to_vec(), r);
                        r
                    }
                }),
            })
            .collect();
        let protos: Vec<ProtoRef> = f
            .protos
            .into_iter()
            .map(|p| self.make_proto(p, source, strings))
            .collect();
        self.heap.alloc(Proto {
            constants: constants.into_boxed_slice(),
            protos: protos.into_boxed_slice(),
            source: Some(source),
            info: Rc::new(f.info),
        })
    }
}
