//! Linking: turning the compiler's plain-data output into heap objects.

use std::cell::Cell as StdCell;
use std::rc::Rc;

use orbit_bytecode::{CaptureSource, Constant, UnlinkedChunk};
use orbit_core::{Cell, Closure, Gc, Proto, Value};

use crate::Vm;

impl Vm {
    /// Link a compiled chunk and return its main function as a closure whose
    /// `_ENV` is the global table. The result is unrooted: call it (or pin
    /// it) before running anything else.
    pub fn load(&mut self, chunk: &UnlinkedChunk) -> Value {
        let name: Rc<str> = Rc::from(&*chunk.name);
        let n = chunk.protos.len();
        // Children come after their parents, so link from the end and every
        // child is ready when its parent is built. Nothing collects here:
        // allocation never runs the collector.
        let mut linked: Vec<Option<Gc<Proto>>> = vec![None; n];
        for i in (0..n).rev() {
            let u = &chunk.protos[i];
            let consts: Vec<Value> = u
                .consts
                .iter()
                .map(|k| match k {
                    Constant::Int(i) => Value::Int(*i),
                    Constant::Float(f) => Value::Float(*f),
                    Constant::Str(b) => Value::Str(self.rt.strings.new_str(&mut self.heap, b)),
                })
                .collect();
            let children = u
                .children
                .iter()
                .map(|&c| linked[c as usize].expect("child prototype comes after its parent"))
                .collect();
            let proto = Proto {
                code: u.code.as_slice().into(),
                consts: consts.into(),
                caches: (0..u.ncaches).map(|_| StdCell::new(u32::MAX)).collect(),
                #[cfg(feature = "type-feedback")]
                feedback: (0..u.code.len()).map(|_| StdCell::new(0)).collect(),
                children,
                captures: u.captures.as_slice().into(),
                nparams: u.nparams,
                is_vararg: u.is_vararg,
                nregs: u.nregs,
                ncells: u.ncells,
                name: u.name.clone(),
                line_defined: u.line_defined,
                chunk: name.clone(),
                debug: Rc::new(u.debug.clone()),
            };
            linked[i] = Some(self.heap.alloc(proto));
        }
        let main = linked[0].expect("chunk has a main function");
        let env = self.heap.alloc(Cell {
            value: Value::Table(self.rt.globals),
        });
        let captures = self
            .heap
            .get(main)
            .captures
            .iter()
            .map(|c| match c {
                CaptureSource::Env => env,
                _ => self.rt.dead_cell,
            })
            .collect();
        Value::Closure(self.heap.alloc(Closure {
            proto: main,
            captures,
        }))
    }
}
