# VM: runtime objects, bytecode, compiler and interpreter

Status: Implemented. Crates: `orbit-runtime` (`object`: the runtime objects; `vm`: the interpreter), `orbit-bytecode` (the instruction set), `orbit-compiler` (HIR to bytecode).

This document covers everything after the [HIR](hir.md): the objects a running program manipulates, the bytecode, the compiler that produces it, and the interpreter that runs it. It builds on ADR-1 (values), ADR-2 (the collector in `orbit-gc`), ADR-3 (stackless frames and continuation-style natives), ADR-4 (bytecode and dispatch), ADR-5 (strings) and ADR-6 (safety). The design is our own: register-based like PUC Lua's VM, with its own instruction set, capture model, error-message mechanism and calling convention. Metatables, userdata, coroutines and to-be-closed variables are in [VM semantics](vm-semantics.md); the debug library and hooks in [debug.md](debug.md).

## Runtime objects (`orbit_runtime::object`)

Every object lives in one `LuaHeap`, a heap of `orbit-gc` with one arena per type:

```rust
orbit_gc::space! {
    pub struct LuaSpace {
        strings: LuaStr,
        tables: Table,
        closures: Closure,
        cells: Cell,
        protos: Proto,
        userdata: UserData,
        threads: Thread,
    }
}

#[repr(C, u8)]                 // pinned layout (ADR-1): tag byte at offset 0, payload at offset 8
pub enum Value {               // 16 bytes
    Nil = 0,
    Bool(bool) = 1,
    Int(i64) = 2,
    Float(f64) = 3,
    Str(Gc<LuaStr>) = 4,
    Table(Gc<Table>) = 5,
    Closure(Gc<Closure>) = 6,
    Native(NativeId) = 7,      // index into the VM's native registry
    UserData(Gc<UserData>) = 8,
    Thread(Gc<Thread>) = 9,
    LightUserData(LightUserData) = 10,
}
```

Handles (`Gc<T>`) are plain indexes, so reading an object takes the heap (`heap.get(t)`). The layout of `Value` is pinned so a JIT could read and write values directly (see [JIT readiness](#jit-readiness)); `#[repr(C, u8)]` and explicit discriminants are plain safe Rust. The tag numbers are also the tag bytes table storage keeps, so a table slot and a register read the same way. Code outside `object` uses `Value`'s API, never its layout.

- **Native functions** are `Value::Native(NativeId)`, an index into a per-VM registry of `{ name, fn, fast path, data }`. They cost no allocation, compare by id (Rust function-pointer equality isn't reliable), and print as `function: 0x...` like any function, as the reference's do. Natives with upvalues are native closures ([VM semantics](vm-semantics.md#coroutines)).
- **Strings** (`LuaStr`, ADR-5): byte strings, stored inside the heap slot up to 22 bytes and in their own allocation beyond. Strings up to 40 bytes are interned in an `Interner`, a hash set of handles keyed by a seeded hash of the bytes (linear probing, at most 3/4 full, 8-byte slots holding the hash and the handle, hash 0 marking an empty slot). Equal short strings are the same handle, so key comparison is a handle compare; long strings are hashed lazily the first time they are used as a key and compare by bytes. The interner is weak. After a minor collection the VM's `Roots::clear_weak` removes the young strings that died. After a full marking it prunes in steps (`Roots::sweep_weak`) before the sweep frees anything, and a lookup that finds a dead string not yet removed revives it (`Heap::revive`). When pruning ends, the set moves to a smaller one if what is left would fill a quarter of it.
- **`OrbitString` / `OrbitStr<'a>`** (ADR-5), in `object/orbit_str.rs` and re-exported by `orbit`, are how error messages and strings travel between natives, embedders and the runtime.
- **Tables** (`Table { array, entries, meta, .. }`):
  - The array part stores tags and payloads in one allocation (ADR-1): `cap` payload words followed by the tag bytes, eight to a word, 9 bytes per slot. The words are plain `[u8; 8]` arrays private to `ArrayPart`, whose API takes and returns `Value`s; a payload is read and written as one native-endian `u64` and a tag as one byte, so a store never reads, masks and rewrites the tag word. `Value::pack` and `Value::unpack` convert to and from `(Tag, Payload)` with no `unsafe`. `Tag` is a one-byte enum with the pinned tag numbers, so a stored tag is always valid, and `Payload` is an opaque 8-byte newtype that only `pack` makes and only `unpack` reads, so packed storage keeps `Value`'s type safety. An integer-keyed read checks for an integer or a float with plain compares before the general case.
  - The hash part is our own open-addressing table with linear probing and power-of-two capacity. An entry is 24 bytes: key and value each split into tag and payload like the array part (the two tag bytes side by side), and the key's hash. Parts of up to 8 slots may fill completely, so a constructor's hint of `n` allocates exactly `n` rounded up to a power of two; larger parts stay at most 3/4 full. Keys are normalised on the way in: a float with an integer value becomes `Int`, `nil` is `table index is nil`, NaN is `table index is NaN`. Assigning `nil` keeps the key in place with a `nil` value (a dead entry), so `next` keeps working while a loop clears the table; dead entries are dropped when the table is rebuilt.
  - Integer key `n+1` goes to the array part when it extends it, and keys that then continue the array move over from the hash part. A rebuild (on hash-part growth) sizes the array part by density: the largest power of two `2^b` such that more than half of the keys `1..=2^b` would be present (more than a third from `2^12` slots up, where an array slot's 9 bytes already cost less than a hash entry, so a large table filled in no particular order moves to the array part before its hash part doubles again; small parts keep Lua's half so `#` finds the same borders). Keys are tallied per power-of-two range rather than sorted. The array part never shrinks; when it grows, its length becomes the largest present key up to `2^b`, the hash part's keys in range move into it, and missing keys are nil holes. The rebuild allocates everything it needs before it moves anything, so running out of memory leaves the table unchanged, and the heap-limit check estimates exactly those allocations.
  - `#t` returns a border: the array length when its last slot is non-nil and `n+1` is absent from the hash part, otherwise a binary search inside the array part, or a search upward through the hash part.
  - `next` visits array slots, then hash slots. It remembers the hash slot of the key it last returned (`next_hint`), and when the key it is given is that slot's key, it resumes there without hashing and probing.
  - `absent_meta` remembers, one bit per metatable field, that the table is known to lack that field, so a metamethod lookup that already found nothing is a bit test the next time. Every store into the hash part clears it.
  - Small array and hash parts of dead tables go to a per-thread pool (at most 64 KiB) that new tables take from first, which is cheaper than returning a sweep's worth of small blocks to the system allocator and getting them back.
  - **Two-phase mutation.** Finding a key needs to read string objects, and `Heap::update` holds the heap mutably, so lookups and writes are split: `find_slot` borrows the heap immutably, then `heap.update(table, |t| t.write(slot, value))` performs the store and applies the write barrier. Only a table that must grow rebuilds itself inside `update`.
- **Closures** (`Closure { proto, code, captures }`) and **cells** (`Cell { value }`, see [below](#locals-captures-and-cells)). `code` is an `Rc` copy of the prototype's code, so a call reaches the code and frame sizes with one heap lookup.
- **Prototypes** (`Proto`) are GC objects holding the linked output of the compiler: `code: Rc<ProtoCode>`, `children`, `captures` (where each capture comes from), the function's name and lines, the chunk name and source, the capture `CapSelf` stands for, and `debug: Rc<DebugInfo>`. `ProtoCode` is what running the function needs: the instructions, constants and inline caches, `nparams`, `is_vararg`, `nregs`, `ncells`, and two flags for built-ins written in OrbitScript (`builtin`, shown as a C function by the debug interface, and `refuses_yield`).
- **Numbers**: `object::number` holds string-to-number conversion (the literal grammar plus surrounding whitespace, used by `tonumber` and coercion) and number-to-string formatting. Floats format as Lua 5.5 does: `%.15g` when that reads back as the same float, else `%.17g`, then `.0` appended if the text looks like an integer; `inf`, `-inf`, `nan`, `-nan`. Rust's own `{}` formatting differs (`1e+16` against `10000000000000000`), so this is our own `%g`, tested against reference outputs.

## Locals, captures and cells

The capture model is the main departure from how PUC Lua works.

A local that no closure captures (the HIR's `Local::captured == false`, most locals) lives in a register. A **captured local lives in a heap `Cell`** from the moment its declaration executes. The declaring function reaches it through a per-frame cell slot, and every closure that captures it holds the same `Gc<Cell>`. There are no "open upvalues" pointing into the stack, and nothing to close when a scope ends.

- Fresh bindings per iteration and per `local` statement fall out for free: executing the declaration allocates a new cell (`CellNew`), and closures created earlier keep the old one. `CellClear` drops the cells of locals going out of scope, so the slots don't keep them alive.
- The main chunk's `_ENV` is a cell too, created by the loader and holding the globals table. It is capture 0 of any function that uses globals.
- Each stack has two arrays: `regs: Vec<Value>` and `cells: Vec<Gc<Cell>>`. Keeping cells out of `regs` means `Value` needs no hidden internal variant. When a collection cycle ends at a safe point and the running stack's `regs` holds more than 64K slots of capacity with at most a quarter in use (a native pushed a large table's elements, say, or recursion went deep), it shrinks to twice what is in use.
- Costs: a captured local costs one small allocation per binding (a closure allocation usually happens in the same scope anyway), and the declaring function reads it through the cell. In return there is no open-upvalue list, no close-on-return scan, returns are cheaper, coroutines need no upvalue fix-ups, and a JIT sees plain memory cells.
- `debug.getlocal` and `debug.setlocal` read and write a captured local through its cell.

## Bytecode (`orbit-bytecode`)

The instruction set lives in its own dependency-free crate, because the compiler emits it, `Proto` stores it and the VM runs it; that keeps the compiler free of the runtime and the GC with no crate cycle. The crate is the exact contract; this section summarises it.

Register-based, one Rust enum, **8 bytes per instruction** (ADR-4):

```rust
#[derive(Clone, Copy)]
pub enum Op {
    Move { dst: Reg, src: Reg },
    LoadInt { dst: Reg, value: i32 },
    // ...
}
const _: () = assert!(core::mem::size_of::<Op>() == 8);
```

A Rust enum instead of hand-packed words: the compiler decodes fields for us, the `match` is exhaustive, and there is no bit-twiddling to get wrong. Register operands are `u8` (at most 255 registers per function, as in Lua 5.5); constant and child-prototype indexes are `u16`, and a function may hold up to 2^25 constants, past the first 65,536 loaded with `LoadConstX` and used through registers. Jump offsets are `i32`, relative to the next instruction. `MULTI` (255) as a count means "up to the top".

`R[x]` is a register, `K[x]` a constant, `C[x]` a frame cell slot, `U[x]` a capture of the current closure:

| Group | Instructions |
|---|---|
| Loads | `Move`, `LoadNil{dst,count}`, `LoadBool`, `LoadInt{dst,value:i32}`, `LoadConst{dst,k}`, `LoadConstX{dst,k:u32}` |
| Cells and captures | `CellNew{cell,src}` (a fresh cell holding `R[src]`), `CellGet`, `CellSet`, `CellClear{from,count}`, `CapGet{dst,cap}`, `CapSet{cap,src}`, `CapSelf{dst}` (the running closure, for a `local function` that refers to itself and is never reassigned) |
| Globals | `GetGlobal{dst,name:k,ic}`, `SetGlobal{src,name:k,ic}` (through `U[0]`, the `_ENV` cell), `CheckUndefined{src,name:k}` (Lua 5.5's `global x = v`) |
| Tables | `NewTable{dst,array_hint,hash_hint}`, `GetField{dst,obj,key:k,ic}`, `SetField{obj,key:k,src,ic}`, `GetIndex`, `SetIndex`, `GetIndexInt` and `SetIndexInt` (integer constant keys), `SetList{table,first,count,start}`, `SelfCall{dst,obj,key:k,ic}` (method lookup: `R[dst+1] = R[obj]; R[dst] = R[obj][K[key]]`) |
| Arithmetic | `Add`, `Sub`, `Mul`, `Div`, `IDiv`, `Mod`, `Pow` as `{dst,a,b}`; the same with a constant right operand (`AddK` ... `PowK`) or left operand (`KAdd`, `KSub`, `KMul`, `KDiv`); `AddInt` and `SubInt` with an `i16` immediate; `Neg`, `Not`, `Len`, `Concat{dst,first,count}` |
| Bitwise | `BAnd`, `BOr`, `BXor`, `Shl`, `Shr` as `{dst,a,b}`, `BNot{dst,src}` |
| Branches | `Jump{off}`, `JumpIfTrue`, `JumpIfFalse`, and fused compare-and-branch instructions `JumpLt`, `JumpNotLt`, `JumpLe`, `JumpNotLe`, `JumpEq`, `JumpNe` as `{a,b,off}`; against a small integer literal, `JumpEqInt`, `JumpLtInt`, `JumpLeInt`, `JumpGtInt`, `JumpGeInt` as `{a,imm:i8,want,off}` (jump when the comparison's result equals `want`, so one instruction covers both branch directions and stays correct for NaN) |
| Calls | `Call{func,nargs,nresults}`, `TailCall{func,nargs}`, `Return{first,count}`, `Return0`, `Return1{src}` |
| Varargs | `Vararg{dst,count}`; for named varargs, `VarargTable{dst}` (build the table at entry), `VarargGet`, `VarargLen` and `VarargGetInt` (read the extra arguments as the table would, without building it), `VarargUnpack{dst,table,count}` (`...` from a table that was built) |
| Loops | `ForPrep{base,exit}`, `ForPrepExclusive{base,exit}` (OrbitScript's `a..b`), `ForLoop{base,back}` (integer and float variants chosen at `ForPrep`), `GenericCall{base,nvars}`, `GenericLoop{base,back}` |
| To-be-closed | `Tbc{reg}`, `Close{from}` ([VM semantics](vm-semantics.md#to-be-closed-variables)) |
| Casts | `Cast{dst,src,ty,mode}`: OrbitScript's `as` ([runtime-as.md](orbitscript-syntax/runtime-as.md)). `mode` names the target (`int`, `float`, `number`, `string`, `bool`, `nil`, a struct or enum in `R[ty]`, or a failure naming `R[ty]`) plus flags: `nil` passes, `nil` on failure (`as?`), or only test the type and give a boolean (union members). `int` on an integer is inline; the rest is out of line, and only a conversion to a string allocates or calls `__tostring` |
| Functions | `Closure{dst,proto}` (captures built from the child prototype's capture sources: a cell slot of the current frame or a capture of the current closure), `NativeEntry{id}` (the body of a native closure's stub prototype; never emitted by a compiler) |

**Inline caches.** `ic` operands index a per-prototype array of `FieldCache`s (ADR-4). A cache holds its instruction's string key and that key's hash, filled in at link time, and the hash slot where the key was last found; a hit is "the slot still holds this key", so the hit path reads no constant, and a miss falls back to the normal lookup and refreshes the slot. A function with more sites than cache indexes shares the last cache among them; linking marks a cache whose sites disagree on the key as shared, and a shared cache never fills, so those sites always take the miss path with their own key. `SelfCall` has a chain of caches: the method in the object itself, in its metatable's `__index` table (a class), or one level further up (a superclass).

Compare-and-branch fusions save about a seventh of the instructions on branchy code. The constant and immediate operand forms came from profiles: in fib and fannkuch, a `LoadInt` before every `n < 2` or `p[1]` was about a tenth of the instructions run. New instructions need rows in `format.rs` (binary chunks) and arms in the verifier ([chunks-io-os-cli.md](chunks-io-os-cli.md#the-verifier)); the matches are exhaustive, so a missing arm doesn't compile.

**Type feedback (not built).** A JIT would want a feedback byte per instruction for the instructions whose operand types it specialises on (arithmetic, comparisons, `Len`, `Concat`, indexing and calls), recording what the interpreter saw. It belongs in a side table indexed by pc rather than in an operand, so no instruction grows past 8 bytes. It is added with the JIT and its recording.

### Debug info

Kept beside the code (ADR-4), in `DebugInfo`:

- **Lines**: run-length pairs `(first_pc, line)`, binary-searched.
- **Locals**: name, register or cell slot, and the live pc range, for tracebacks and `debug.getlocal`.
- **Capture names**, for `upvalue 'x'` messages, and the debug order of upvalues ([debug.md](debug.md#upvalues)).
- **Operand origins** (see [Error messages](#error-messages)): for each instruction that can raise a type error, where its operands came from.

## Compiler

`orbit-compiler` compiles one HIR function at a time into an `UnlinkedProto`: plain data (`Send`) with the instructions, constants as `Int | Float | Str(bytes)`, child function ids, capture sources, counts and debug info. Linking into a VM heap (interning strings, allocating `Proto` objects bottom-up) is a separate step in the VM (`vm/link.rs`), so the compiler never needs the GC.

- **Registers** are allocated in stack order: parameters first, then each block's locals in declaration order, released when the block ends, and temporaries above the live locals. Reading an uncaptured local uses its register directly, with no `Move`. Captured locals get a cell slot instead, allocated the same way.
- **Expressions** compile with a destination hint (any register, or a given one). Constants that fit use `LoadInt` or an immediate operand; everything else goes in the constant pool, deduplicated.
- **Multiple results**: one function places an expression list in consecutive registers, asking a trailing call or `...` for exactly the missing count or for all of them (`MULTI`). It serves calls, returns, `local`, assignment and table constructors.
- **Conditions** compile straight to branches: `not`, `and`, `or` and comparisons become fused `Jump*` instructions without materialising booleans. `a > b` evaluates `a` then `b` and emits `JumpLt b, a`.
- **Jumps** use forward labels with a pending-patch list, resolved when the label is placed. `goto` is a `Jump`; registers are allocated by block, so a jump between block levels needs no register work, and a backward `goto` to before a captured local's declaration gets a fresh cell on the next pass.
- **Constant folding** of arithmetic and bitwise operations on numeric literals, only where the result can't raise an error (no folding `1 // 0`), and constant propagation of `Const` and `Normal` locals that are never reassigned.
- **Tail calls**: `Return` of a single call becomes `TailCall`, always followed by `Return{func, MULTI}`, which passes the results on when the callee turns out to be a native that asks for a call (`CallThen`).
- **Assignment order** follows the HIR's rule (targets' objects and keys, then values, then stores right to left).
- **Limits** give Lua's compile errors: more than 255 registers is `function or expression needs too many registers`, more than 254 returned values is `too many returns (limit is 255)`, and more than 255 captures is `too many upvalues`.
- A disassembler (`orbitc -l`) prints instructions with operand names from the debug info, for golden tests.

## Interpreter (`orbit_runtime::vm`)

### State

```rust
pub struct Vm {
    pub heap: LuaHeap,
    pub rt: Runtime,       // everything else, so the heap can be borrowed mutably
}                          // while the runtime is borrowed as its GC roots

pub struct Runtime {
    pub strings: Interner,
    pub globals: Gc<Table>,
    pub registry: Gc<Table>,   // key 2 holds the globals; `_LOADED` is package.loaded
    pub limits: Limits,
    pub out: Box<dyn WriteDebug>,                      // where `print` writes, buffered
    pub compile: Option<CompileFn>,                    // the Lua front end, installed by `orbit`
    pub compile_orbitscript: Option<CompileFn>,        // the OrbitScript front end
    natives: Vec<Native>,
    stack: Stack,          // the running thread's stack
    // ... metatables, coroutine state, caches, warnings, extensions
}

struct Stack {
    regs: Vec<Value>,
    cells: Vec<Gc<Cell>>,
    frames: Vec<Frame>,
    hook: Hook,            // debug.sethook, per thread
    // ...
}

struct Frame {
    kind: FrameKind,       // Lua, Native (waiting on a call it asked for), Meta, Handler,
                           // Boundary, CoBase, NativeCall, Hook
    func: u32,             // where the callee was; results go here
    base: u32,             // first register (Lua) or first argument (native)
    pc: u32,
    cell_base: u32,
    nvarargs: u32,         // extra arguments, stored just below `base`
    want: u8,              // results the caller expects (MULTI = all)
    tail: bool, extra: u8, cdepth: u8,   // tail call, `__call` chain, nested-call depth
}
```

The frame kinds beyond `Lua` and `Native` are described in [VM semantics](vm-semantics.md) and [debug.md](debug.md).

### Dispatch

`Vm::run` is one loop over the current frame's code (ADR-4), in `vm/dispatch.rs`:

```rust
loop {
    let op = code[pc];
    pc += 1;
    match op {
        Op::Add { dst, a, b } => {
            match (&w[a], &w[b]) {
                (Value::Int(i), Value::Int(j)) => set_int(&mut w[dst], i.wrapping_add(*j)),
                (Value::Float(f), Value::Float(g)) => set_float(&mut w[dst], f + g),
                _ => /* out of line: coercions, metamethods, errors */,
            }
        }
        // ...
    }
}
```

The loop keeps the running frame's state in locals (base, pc, code and a fixed-size view of its registers) and runs the common instructions in place. Lua-to-Lua calls and returns switch that state in place; everything else that changes the frame stack comes back through one `'reload` point, which reads the state from the frame on top.

- **Register window.** While a Lua frame runs, `regs` is kept at least `base + 256` long. The loop borrows `regs[base..base + 256]` as a `&mut [Value; 256]`, and register numbers are `u8`, so register access needs no bounds check and the window's address stays in a machine register. Anything that takes `&mut self` (calls, allocation, collection steps, errors) ends the borrow, and the loop takes the window again afterwards. Values past the window, such as open-ended call results or varargs, are reached through `regs` itself.
- **Read registers in place.** An instruction matches on its operand registers where they are and never copies them to a temporary `Value` first. Arithmetic writes its result as a tag byte and an 8-byte payload, two stores. A following instruction that loaded the whole 16-byte value couldn't take it from the store buffer and would stall until both stores land (about 10 to 15 cycles on x86). Matching in place loads just the tag and the payload, which the store buffer forwards. This cut nbody by 25% and spectral_norm by 38% on x86_64.
- **Copy numbers by parts.** The same stall hits instructions that copy a value (`Move`, table, field and cell stores, `Return1`). They match the source in place and, for an integer or a float, store the known variant, so the copy is again a tag store and a payload store. A `for` loop's counters are updated by changing their payloads in place.
- **Hot and cold halves.** The loop is one large function, and the compiler allocates machine registers across all of it, so an arm holds only an instruction's common case: decoding, a type check and a few register operations. Everything slower, rarer or larger lives out of line behind `#[inline(never)]`: `vm/cold.rs` (allocation, cache misses, varargs, casts, cells, `for` setup), `vm/fast.rs` (the generic halves of arithmetic and comparisons), native calls, and the error path (`Vm::raise`). New instructions follow the same split, which keeps the loop's size, and the hot instructions' register allocation, independent of how many instructions there are. Compare changes by instruction counts (callgrind) as well as wall time, since code movement shifts wall time by a few percent through alignment and branch prediction.
- **Two loops from one source.** With the `hooks` feature, `run_loop` is generic over whether hooks are on, and the hooked copy runs only where a hook, breakpoint or step needs it ([debug.md](debug.md#hooks)).

### Calls and returns (stackless, ADR-3)

- `Call{func, nargs, nresults}`: the callee is in `R[func]`, the arguments above it. For a Lua closure, the new frame's `base` is `func + 1` and missing parameters are `nil`. For a vararg function, the extra arguments stay where they are and the frame's registers start above them, with the fixed parameters moved up; `Vararg` copies from below `base`. There is no Rust recursion: the loop saves `pc`, pushes the frame, and switches its state.
- `Return{first, count}` copies results to where the callee was (`R[func]` of the caller), pads or truncates them to the caller's `want`, pops the frame and switches back. When `want` is `MULTI`, the caller's top is set for the following `Call`, `Return` or `SetList` that consumes all values.
- `TailCall` replaces the current frame, so unbounded tail recursion runs in constant space, as Lua requires.
- **Fast switches.** A call to a Lua closure, and a `Return1` to a Lua caller that wants one value, switch the loop's frame state in place from values already in hand. The code is only reloaded when it changes, so a recursive call or its return never touches it, and the code that ran before the last switch is kept aside: a loop calling another function goes back and forth between two codes, so each switch swaps them instead of looking the closure up again. A closure calling itself skips the heap lookup for its prototype, and any other Lua callee goes through a one-entry cache of the last closure called and what entering it needs (its code's address and its parameter, register and cell counts). The cache is a GC root, so while an entry is held its handle can't be reused for another closure.
- **Self reference.** A `local function` that calls itself would read its own name through a capture, two arena lookups. When the local is never assigned, the compiler emits `CapSelf` instead, which loads the running closure from the loop's frame state. The `local function` statement stores the closure in the cell before anything can call it, so the two agree; the debug library keeps them agreeing when it writes the cell ([debug.md](debug.md#capself)).
- **Depth limit**: pushing a frame beyond the configured limit (one million register slots by default) raises the catchable error `stack overflow`. The Rust stack never grows with Lua recursion.

### Native functions

```rust
pub type NativeFn = fn(&mut NativeCtx<'_>) -> Result<NativeReturn, LuaError>;
pub type FastFn = fn(&LuaHeap, &[Value]) -> Option<Value>;
pub type Continuation = fn(&mut NativeCtx<'_>, CallOutcome) -> Result<NativeReturn, LuaError>;

pub enum NativeReturn {
    Return(usize),                                               // the last n values of the window
    CallThen { f: Value, nargs: usize, k: Continuation, protected: bool },
    CallThenYieldable { f: Value, nargs: usize, k: Continuation, protected: bool },
    CallHandled { f: Value, nargs: usize, k: Continuation },     // xpcall
    Resume { co: Gc<Thread>, nargs: usize, k: Continuation, .. },
    // ... yield, close
}
```

`NativeCtx` is the native's window of the register stack: its arguments first, then whatever it pushes. It has argument access and checks (`arg`, `check_str`, `check_int`, with Lua's `bad argument #2 to 'format' (...)` messages, the function name coming from the call site), window editing (`set`, `push`, `truncate`), allocation, and metatable helpers. A native that needs to call a Lua function doesn't call back into the VM: it returns `CallThen`, the VM runs the call in its own loop, then invokes the continuation with the results or the error. The call's results replace its arguments at the end of the window, and state the native needs afterwards stays in its window, where the collector sees it. That is ADR-3's continuation style, and it is how `pcall` works:

```rust
fn pcall(ctx) -> ... { Ok(CallThenYieldable { f: ctx.arg(0)?, nargs: ctx.nargs() - 1, k: pcall_done, protected: true }) }
fn pcall_done(ctx, outcome) -> ... { match outcome { Ok(n) => push true + n results, Err(e) => push false, e } }
```

Code called through `CallThen` may not yield across it (`attempt to yield across a C-call boundary`), matching most Lua 5.5 library callbacks; `CallThenYieldable` is for the few the reference lets callbacks yield through (`pcall`, `pairs` calling `__pairs`, `dofile`).

**Fast paths.** A native may also have a `FastFn`. A call that wants at most one result tries it first, with the arguments as a slice of the caller's registers: no window, no frame, no result copying. It returns `None` for anything it doesn't handle (wrong types, coercions, errors), and the full native then runs, so a fast path never changes behaviour. It can't allocate or call Lua, which keeps it a plain function a JIT could call directly. Most of `math` and the intrinsics the OrbitScript built-ins call have one.

**Native data.** A native also carries two data words (`NativeCtx::native_data`), for natives that are many functions at once, such as an FFI binding's address and signature ([ffi.md](ffi.md)).

### Errors

A Lua error is `Err(LuaError)` propagated with `?`, never a Rust panic. Slow paths return a small `Exc` (a runtime error with its operand origin, a Lua error value, or a metamethod request) through the same channel, so the fast paths stay unchanged. When the loop sees an error it unwinds ([VM semantics](vm-semantics.md#xpcall-and-errors)): it pops frames until it reaches one that catches (a protected native continuation, a `Boundary` or a `CoBase`), closing to-be-closed variables on the way, truncates `regs` and `cells` to that frame, and resumes it with the error. With no catching frame left, the error returns to the embedder with a traceback built before any frame was popped.

`error(msg, level)` adds `chunk:line:` for the activation at `level` (1 is the caller of `error`, 2 its caller), from the line table at that frame's saved `pc`. `error()` with no value raises `nil`, which the CLI prints as `<no error object>`.

### Error messages

The reference names the culprit in type errors: `attempt to index a nil value (field 'a')`. We produce the same text from **compile-time knowledge**: when the compiler emits an instruction that can fail on an operand, it records where that operand came from (`Origin::{Local, Global, Field, Upvalue, Method, Constant}`) in the debug info, keyed by pc and operand. On an error, the slow path looks up the pc and appends the description. Nothing analyses bytecode after the fact, and OrbitScript's front end can record its own origins the same way.

Covered forms include `attempt to index a nil value (local 't')`, `attempt to call a nil value (global 'f')`, `(method 'm')`, `(upvalue 'u')`, `(field 'a')`, `(constant 'x')`; `attempt to perform arithmetic on a table value (local 's')`, `attempt to add a 'string' with a 'number'`, `attempt to concatenate a table value`, `attempt to get length of a nil value`; `attempt to compare number with string`, `attempt to compare two table values`; `attempt to divide by zero`, `attempt to perform 'n%0'`, `table index is nil`, `table index is NaN`; `'for' step is zero`, `bad 'for' initial value (number expected, got string)`, `stack overflow`.

### Garbage collection

- **Roots** (`impl Roots<LuaSpace> for Runtime`): the running stack's registers, cells and frames; parked coroutine stacks are traced through their `Thread` objects; the globals, the registry, the per-type metatables, interned metatable field names, the call cache, and the loader's in-progress prototypes. `clear_weak` and `sweep_weak` prune the string interner.
- **Safe points**: after instructions that allocate (`NewTable`, `Closure`, `CellNew`, `Concat`, table stores that grow, native calls that allocated), the loop checks whether the collector wants to step and runs `heap.step`. A store into an existing array slot allocates nothing and skips the check. At a safe point every live handle is in a register, cell slot, frame or root: the loop itself holds only an `Rc<ProtoCode>` (whose constants the frame's closure keeps alive) and integers. Natives never hold handles across `CallThen`; state they need afterwards goes in their window.
- **Barriers**: table stores go through `Heap::update` (two-phase, above), and so do `CellSet`, `CapSet` and `SetGlobal`. The running stack is a root and is rescanned when marking finishes, so register writes need no barrier.
- **Memory release**: `orbit-gc`'s `memory-trimming` feature (on by default) returns freed pages to the OS after cycles that release them (`malloc_trim` on glibc, `malloc_zone_pressure_relief` on macOS).
- **Start floor**: in generational mode no major cycle starts below 1 MiB of heap; `ORBIT_GC_MIN_THRESHOLD` (bytes, or with `K`, `M`, `G`; `0` for none) changes it (ADR-2).
- **Stress mode**: `ORBIT_GC_STRESS=1` collects at nearly every safe point that follows an allocation; the differential tests run under it as well as normally.

Weak tables, finalizers and `collectgarbage` are in [gc-semantics.md](gc-semantics.md).

## JIT readiness

A JIT (an optional Cargo feature, in a future `orbit-jit` crate) would compile hot functions from this bytecode, not from the HIR. The bytecode is where runtime type feedback lives, and HIR type annotations are optional and never enforced, so they can't be trusted for code generation. The design leaves these hooks for it:

| JIT need | Where the design provides it |
|---|---|
| Translate a function | Register bytecode maps directly onto code-generator variables; fused compare-and-branch instructions become native branches |
| Read and write values | `Value` has a pinned layout: tag byte at offset 0, 8-byte payload at offset 8, fixed tag numbers shared with table storage |
| Specialise on types | Per-instruction type-feedback bytes, added with the JIT |
| Specialise lookups | Inline-cache slots on global, field and method instructions, readable as guards ("key still in the cached slot") |
| Uncommon cases | The out-of-line halves of instructions and the native functions, called from JIT code as helpers |
| Fall back to the interpreter | Frames record closure, pc, base and cell base (ADR-3). JIT code keeps Lua registers in `regs` (or spills them there before any exit), so leaving JIT code is "store pc, return to the loop" |
| Captured variables | Heap cells: a load or store through the cell, with no open-upvalue bookkeeping |
| Compile off-thread | The compiler never needs the heap; a JIT takes a copy of a prototype's code, constants and feedback, since `Rc` can't cross threads |

**The JIT–VM contract**, which `orbit-jit` (allowed `unsafe` by ADR-6) must follow:

1. **Store pc before every helper call.** Error messages, tracebacks and `error` levels all come from the frame's pc and the debug info.
2. **Reload after helpers that can move memory.** Anything that allocates can run a GC step, which can release arena pages, and a call can grow `regs` (a `Vec`, which may reallocate). After such a helper, JIT code reloads the register base and every arena's slot base. An arena's slots are one buffer in `orbit-gc` (ADR-2), which allocation can grow and a sweep can shrink; between helper calls nothing moves, so direct loads through a slot base are valid.
3. **Handles stay handles.** A `Gc<T>` is an index, not a pointer. JIT code turns it into an address with the arena's slot base, which needs `orbit-gc` to expose the slot base and pin the layout of `Gc<T>` and of its slots.
4. **Yields leave JIT code.** Coroutine switches and native `CallThen` requests return to the interpreter loop, which handles them as it already does. JIT code never holds Rust frames across a yield.
5. **Barriers.** Stores into tables and cells from JIT code go through the same write-barrier helpers, or inline the "is the owner black?" check once `orbit-gc` exposes it.
