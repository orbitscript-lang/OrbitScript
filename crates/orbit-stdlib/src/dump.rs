//! `string.dump`: a Lua function as a binary chunk in our own format
//! (`orbit_bytecode::format`), which `load` reads back after verifying it.

use std::collections::VecDeque;

use orbit_vm::op::{CaptureSource, Constant, UnlinkedChunk, UnlinkedProto, format};
use orbit_vm::orbit_core::{Gc, LuaHeap, Proto, Value};
use orbit_vm::{LuaError, NativeCtx, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, orbit_vm::NativeFn)] = &[("string.dump", dump)];

fn dump(ctx: &mut NativeCtx<'_>) -> R {
    let Value::Closure(c) = ctx.arg(0) else {
        return Err(ctx.arg_error(0, "Lua function expected"));
    };
    let strip = !ctx.arg(1).is_falsy();
    let root = ctx.heap.get(c).proto;
    let chunk = unlink(ctx.heap, root);
    let bytes = format::encode(&chunk, strip);
    let s = ctx.new_str_owned(bytes);
    ctx.ret1(s)
}

/// Rebuild the plain-data chunk whose main function is `root`. Prototypes
/// are numbered breadth-first, so children come after their parents.
pub(crate) fn unlink(heap: &LuaHeap, root: Gc<Proto>) -> UnlinkedChunk {
    let name: Box<str> = heap.get(root).chunk.as_ref().into();
    let mut protos = Vec::new();
    let mut queue = VecDeque::from([root]);
    while let Some(p) = queue.pop_front() {
        let p = heap.get(p);
        let code = &p.code;
        let first_child = protos.len() + queue.len() + 1;
        let children = (0..p.children.len())
            .map(|i| (first_child + i) as u32)
            .collect();
        queue.extend(p.children.iter().copied());
        protos.push(UnlinkedProto {
            name: p.name.clone(),
            line_defined: p.line_defined,
            // Linked prototypes don't keep it yet.
            last_line: 0,
            nparams: code.nparams,
            is_vararg: code.is_vararg,
            nregs: code.nregs,
            ncells: code.ncells,
            ncaches: code.caches.len() as u16,
            code: code.code.to_vec(),
            consts: code
                .consts
                .iter()
                .map(|&k| match k {
                    Value::Int(i) => Constant::Int(i),
                    Value::Float(f) => Constant::Float(f),
                    Value::Str(s) => Constant::Str(heap.get(s).as_bytes().into()),
                    _ => unreachable!("constants are numbers and strings"),
                })
                .collect(),
            children,
            captures: p.captures.to_vec(),
            self_capture: p.self_capture,
            debug: (*p.debug).clone(),
        });
    }
    // The dumped function becomes a main function, and its captures' old
    // sources mean nothing without its parent. Loading gives the first
    // capture the environment and every other one a fresh cell, so none
    // may say `Env`.
    let main = &mut protos[0];
    // Its own closure no longer lives in any capture once loaded.
    main.self_capture = None;
    main.captures = (0..main.captures.len())
        .map(|i| CaptureSource::Capture(i as u8))
        .collect();
    let source = heap.get(root).source.as_ref().into();
    UnlinkedChunk {
        name,
        source,
        protos,
    }
}
