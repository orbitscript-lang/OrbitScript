# VM: runtime objects, bytecode, compiler and interpreter (first light)

Status: Proposed. Crates: `orbit-core` (objects), `orbit-compiler` (HIR to bytecode), `orbit-vm` (interpreter), `orbit-stdlib` (the subset's library), `orbit` and `orbit-cli` (glue). Scope: the [first-light subset](first-light-subset.md).

This document covers everything after the [HIR](hir.md): the objects a running program manipulates, the bytecode, the compiler that produces it, and the interpreter that runs it. It builds on the accepted ADRs (1 values, 2 own GC in `orbit-gc`, 4 `match` dispatch with superinstructions and inline-cache slots, 5 byte strings with `OrbitString`/`OrbitStr`, 6 safety) and the proposed ADR-3 (stackless frames, continuation-style natives). It is our own design: register-based like PUC Lua's VM, but with its own instruction set, capture model, error-message mechanism and calling convention.

## Runtime objects (`orbit-core`)

`orbit-core` defines the Lua object types on top of `orbit-gc` (the API on draft PR #6: `space!`, `Gc<T>`, `Trace`, `Heap::{alloc, get, update, barrier_*, step}`). It takes no dependencies besides `orbit-gc` and `orbit-bytecode` (for `Proto`'s code).

```rust
orbit_gc::space! {
    pub struct LuaSpace {
        strings: LuaStr,
        tables: Table,
        closures: Closure,
        cells: Cell,
        protos: Proto,
    }
}

#[derive(Clone, Copy)]
#[repr(C, u8)]                 // pinned layout (ADR-1): tag byte at offset 0, payload at offset 8
pub enum Value {               // 16 bytes
    Nil = 0,
    Bool(bool) = 1,
    Int(i64) = 2,
    Float(f64) = 3,
    Str(Gc<LuaStr>) = 4,
    Table(Gc<Table>) = 5,
    Closure(Gc<Closure>) = 6,
    Native(NativeId) = 7,      // u32 index into the VM's native registry
    // 8 = UserData, 9 = Thread: reserved for when they arrive
}

const _: () = assert!(core::mem::size_of::<Value>() == 16);
```

The layout is pinned so a future JIT can read and write values directly (see [JIT readiness](#jit-readiness)). `#[repr(C, u8)]` and explicit discriminants are plain safe Rust. The tag numbers are also the tag bytes the table array part stores, so a JIT reads a table slot and a register the same way. Rust code outside `orbit-core` still uses `Value`'s API, never its layout.

- **Native functions** are `Value::Native(NativeId)`, an index into a per-VM registry of `{ name, fn }`. That answers ADR-1's open question for first light: natives cost no allocation, compare by id (Rust fn-pointer equality isn't reliable), and print as `function: 0x...` like any function, as the reference does. Natives with captured state come with metatables and coroutines.
- **Strings** (`LuaStr { hash: Cell<u32>, bytes: Box<[u8]> }`): strings up to 40 bytes are interned (ADR-5) in an `Interner`, a hash set of handles keyed by a seeded hash of the bytes. The interner is weak: the VM's `Roots::clear_weak` drops entries whose strings died. Longer strings are hashed lazily the first time they are used as a key. Equal short strings are the same handle, so key comparison is a handle compare; long strings compare bytes.
- **Tables** (`Table { array: ArrayPart, hash: HashPart, meta: Option<Gc<Table>> }`):
  - The array part stores tags and payloads in one allocation (ADR-1): `cap` payload words followed by the tag bytes, eight to a word, 9 bytes per slot. The words are plain `u64`s (one allocation holds one element type), private to `ArrayPart`, whose API takes and returns `Value`s; tag bytes are written only from a `Tag`. Growth resizes the allocation in place where the allocator can and moves the tag words up. `Value::pack` and `Value::unpack` convert to and from `(Tag, Payload)`, no `unsafe`. `Tag` is a one-byte enum with the pinned tag numbers, so a stored tag is always valid; `Payload` is an opaque 8-byte newtype that only `pack` makes and only `unpack` reads, so packed storage keeps `Value`'s type safety.
  - The hash part is our own open-addressing table with linear probing and power-of-two capacity. An entry is 24 bytes: the key and the value each split into tag and payload like the array part (the two tag bytes side by side), and the key's hash. Parts of up to 8 slots may fill completely (a probe visits at most every slot), so a constructor's `hash_hint` of `n` allocates exactly `n` rounded up to a power of two; larger parts keep the load factor at most 3/4. Keys are normalised on the way in: a float with an integer value becomes `Int`, `nil` is `table index is nil`, NaN is `table index is NaN`. Assigning `nil` keeps the key in place with a `nil` value (a dead entry), so `next` keeps working while a loop clears the table; dead entries are dropped when the table is rebuilt.
  - Integer keys `1..=n` go to the array part when they extend it; a rebuild (on hash-part growth) moves integer keys from the hash part into the array part when at least half of the resulting array would be used.
  - `#t` returns a border: the array length when its last slot is non-nil and `n+1` is absent from the hash part, otherwise a binary search for a border inside the array part, or a search upward through the hash part.
  - Iteration order for `next`: array slots, then hash slots. `next(t, k)` locates `k` (array index or hash slot) and continues from there.
  - **Two-phase mutation.** Finding a key needs to read string objects, and `Heap::update` holds the heap mutably, so lookups and writes are split: `table.find_slot(&heap, key) -> Slot` borrows the heap immutably (for hashes and long-string bytes), then `heap.update(table, |t| t.write(slot, value))` performs the store, which also applies the write barrier. Inserting a new key uses `find_insert_slot` the same way, and only a table that must grow rebuilds itself inside `update`.
  - `meta` is reserved and always `None` at first light.
- **Closures** (`Closure { proto: Gc<Proto>, code: Rc<ProtoCode>, captures: Box<[Gc<Cell>]> }`) and **cells** (`Cell { value: Value }`, see [below](#locals-captures-and-cells)). `code` is a copy of the prototype's, so a call reaches the code and frame sizes with one heap lookup.
- **Prototypes** (`Proto`) are GC objects holding the linked output of the compiler: `code: Rc<ProtoCode>`, `children: Box<[Gc<Proto>]>`, `captures: Box<[CaptureSource]>`, `debug: Rc<DebugInfo>`. `ProtoCode` is what running the function needs: the instructions, constants, inline caches and type feedback, plus `nparams`, `is_vararg`, `nregs`, `ncells`. The interpreter keeps a clone of the current function's `Rc<ProtoCode>` while it runs, so fetching an instruction or constant doesn't go through the heap. A Lua frame records the address of its `ProtoCode`; when a call or return lands in a frame whose code the loop already holds (recursion, returning to the caller of a leaf), it skips both the heap lookup and the clone.
- **Numbers**: `orbit_core::number` holds string-to-number conversion (the literal grammar plus surrounding whitespace, used by `tonumber` and coercion) and number-to-string formatting. Floats format as 5.5 does, observed from the reference: `%.15g` when that reads back as the same float, else `%.17g`, then `.0` appended if the text looks like an integer; `inf`, `-inf`, `nan`, `-nan`. Rust's own `{}` formatting differs (`1e+16` vs `10000000000000000`), so this is our own `%g` implementation, tested against a table of reference outputs.
- **`OrbitString` / `OrbitStr<'a>`** (ADR-5): first light implements the subset of conversions natives need (`as_bytes`, `to_str`, `From<&str>`, `Display`).

## Locals, captures and cells

The capture model is the one real departure from how PUC Lua works, so it gets its own section.

A local that no closure captures (the HIR's `Local::captured == false`, most locals) lives in a register. A **captured local lives in a heap `Cell`** from the moment its declaration executes. The declaring function reaches it through a per-frame cell slot, and every closure that captures it holds the same `Gc<Cell>`. There are no "open upvalues" pointing into the stack and nothing to close when a scope ends.

- Fresh bindings per iteration and per `local` statement fall out for free: executing the declaration allocates a new cell (`CellNew`), and closures created earlier keep the old one.
- The main chunk's `_ENV` is a cell too, created by the loader and holding the globals table. It is capture 0 of any function that uses globals.
- Frames have two stacks, both on the per-coroutine `Stack`: `regs: Vec<Value>` and `cells: Vec<Gc<Cell>>`. Keeping cells out of `regs` means `Value` needs no hidden internal variant.
- Costs: a captured local costs one small allocation per binding (a closure allocation is happening anyway, usually in the same scope), and the declaring function reads it through the cell. Benefits: no open-upvalue list, no close-on-return scan, returns are cheaper, coroutines don't need upvalue fix-ups, and a JIT sees plain memory cells.
- Later, when `debug.getlocal`/`setlocal` arrive, they read through the cell for captured locals.

## Bytecode (`orbit-vm::op`)

The types live in their own dependency-free crate, `orbit-bytecode` (re-exported as `orbit_vm::op`), because the compiler emits them, `orbit-core`'s `Proto` stores them and `orbit-vm` runs them; that keeps the compiler free of the runtime and the GC with no crate cycle. That crate is the exact contract; the table below is its summary.

Register-based, one Rust enum, **8 bytes per instruction**:

```rust
#[derive(Clone, Copy)]
pub enum Op {
    Move { dst: u8, src: u8 },
    LoadInt { dst: u8, value: i32 },
    // ...
}
const _: () = assert!(core::mem::size_of::<Op>() == 8);
```

A Rust enum instead of hand-packed `u32`s: the compiler decodes fields for us, the `match` is exhaustive, and there's no bit-twiddling to get wrong. Register operands are `u8` (at most 250 registers per function), constant and child-prototype indices `u16`, jump offsets `i32` relative to the next instruction. ADR-4 left the instruction width open; 8 bytes is this design's proposal (S0 found 12-byte instructions cost time; 4-byte packing is a P5 experiment). Recording it in ADR-4 needs Tym's OK.

First-light instruction set (`R[x]` is a register, `K[x]` a constant, `C[x]` a frame cell slot, `U[x]` a capture of the current closure):

| Group | Instructions |
|---|---|
| Loads | `Move{dst,src}`, `LoadNil{dst,count}`, `LoadBool{dst,value}`, `LoadInt{dst,value:i32}`, `LoadConst{dst,k}` |
| Cells and captures | `CellNew{cell,src}` (fresh cell holding `R[src]`), `CellGet{dst,cell}`, `CellSet{cell,src}`, `CapGet{dst,cap}`, `CapSet{cap,src}` |
| Globals | `GetGlobal{dst,name:k,ic}`, `SetGlobal{src,name:k,ic}` (through `U[0]`, the `_ENV` cell) |
| Tables | `NewTable{dst,array_hint,hash_hint}`, `GetField{dst,obj,key:k,ic}`, `SetField{obj,key:k,src,ic}`, `GetIndex{dst,obj,key}`, `SetIndex{obj,key,src}`, `SetList{table,first,count,start}`, `SelfCall{dst,obj,key:k}` (method lookup: `R[dst+1]=R[obj]; R[dst]=R[obj][K[key]]`) |
| Arithmetic | `Add`, `Sub`, `Mul`, `Div`, `IDiv`, `Mod`, `Pow` as `{dst,a,b}`; `AddInt{dst,a,imm:i16}`; `Neg{dst,src}`, `Not`, `Len`, `Concat{dst,first,count}` |
| Branches | `Jump{off}`, `JumpIfTrue{src,off}`, `JumpIfFalse{src,off}`, and fused compare-and-branch superinstructions `JumpLt`, `JumpNotLt`, `JumpLe`, `JumpNotLe`, `JumpEq`, `JumpNe` as `{a,b,off}` |
| Calls | `Call{func,nargs,nresults}`, `TailCall{func,nargs}`, `Return{first,count}`, `Return0`, `Return1{src}`, `Vararg{dst,count}` (`count`/`nargs`/`nresults` = 255 means "up to the top") |
| Loops | `ForPrep{base,exit}`, `ForLoop{base,back}` (integer and float variants chosen at `ForPrep`), `GenericCall{base,nvars}`, `GenericLoop{base,back}` |
| Functions | `Closure{dst,proto}` (captures built from the child prototype's `CaptureSource` list: `Cell(slot)` from the current frame or `Capture(i)` from the current closure) |

`ic` operands index a per-prototype inline-cache array (ADR-4). At first light the cache holds the hash-slot index where the key was last found; a hit is "slot still holds this key", a miss falls back to the normal lookup and refreshes the slot. The operand is in the encoding from day one, so P5 can make caches smarter without changing the format.

**Type feedback.** Each prototype has a feedback byte per instruction (`feedback: Box<[Cell<u8>]>` in `ProtoCode`, indexed by pc) for the instructions whose operand types a JIT would specialise on: arithmetic, comparisons, `Len`, `Concat`, indexing and calls. The interpreter ORs in a bit for what it saw (integer, float, string, table, closure, native, other), which gives a JIT "this `Add` has only ever seen integers". It's a side table rather than an operand, so no instruction grows past 8 bytes. Recording costs one load and store in the fast paths, so it's compiled in only with the `type-feedback` Cargo feature, which the JIT feature will turn on. First light defines the table and the feature; whether recording is on by default is decided with P5's measurements.

Compare-and-branch fusions are the superinstructions S0 measured at about 14% on branchy code. More (immediate operands for `n < 2`, `GetField`+`Call`) are added in P5 from profiles.

### Debug info

Kept beside the code (ADR-4), in `DebugInfo`:

- **Lines**: run-length pairs `(first_pc, line)`, binary-searched on error.
- **Locals**: name, register or cell slot, and live pc range, for tracebacks and later `debug.getlocal`.
- **Capture names**, for `upvalue 'x'` messages.
- **Operand origins** (see [Error messages](#error-messages)): for each instruction that can raise a type error, where its operands came from.

## Compiler (`orbit-compiler`)

`compile(&hir::Chunk, FuncId) -> UnlinkedProto`, one function at a time. The output is plain data (`Send`): instructions, constants as `Int | Float | Str(bytes)`, child function ids, capture sources, counts and debug info. Linking into a VM heap (interning strings, allocating `Proto` objects bottom-up) is a separate step in `orbit-vm`, so functions can be compiled in parallel later and the compiler never needs the GC.

- **Registers** are allocated in stack order: parameters first, then each block's locals in declaration order, released when the block ends; temporaries above the live locals. Reading an uncaptured local uses its register directly, with no `Move`. Captured locals get a cell slot instead, allocated the same way.
- **Expressions** compile with a destination hint: `expr(e, Dest::Any | Dest::Reg(r))`. Constants that fit use `LoadInt`; everything else goes in the constant pool (deduplicated).
- **Multiple results**: `expr_list(list, first_reg, Want::Exact(n) | Want::All)` places values in consecutive registers; if the last item is a call or `...`, it asks it for exactly the missing count or for "all" (255). This one function serves calls, returns, `local`, assignment and table constructors.
- **Conditions** compile straight to branches: `cond(e, jump_if: bool, label)` handles `not`, `and`, `or` and comparisons without materialising booleans, choosing the fused `Jump*` forms. `a > b` evaluates `a` then `b` and emits `JumpLt b, a`.
- **Jumps** use forward labels with a pending-patch list, resolved when the label is placed.
- **Constant folding** of arithmetic on numeric literals, only where the result can't raise an error (no folding `1 // 0`), and constant propagation of `Const` locals that are never reassigned.
- **Tail calls**: `Return` of a single call becomes `TailCall`.
- **Assignment order** follows the HIR's rule (targets' objects and keys, then values, then stores right to left).
- **Limits** give Lua-style compile errors: more than 250 registers is `function or expression needs too many registers`, more than 255 captures is `too many upvalues`.
- A disassembler (`orbit --dump-bytecode`) prints instructions with operand names from the debug info, for golden tests.

## Interpreter (`orbit-vm`)

### State

```rust
pub struct Vm {
    heap: Heap<LuaSpace>,
    strings: Interner,
    globals: Gc<Table>,
    natives: Vec<Native>,
    stack: Stack,            // the running coroutine; a heap object once coroutines exist
    limits: Limits,          // stack depth etc.
    out: Box<dyn Write>,     // `print` target, stdout by default
}

pub struct Stack {
    regs: Vec<Value>,
    cells: Vec<Gc<Cell>>,
    frames: Vec<Frame>,
}

pub struct Frame {
    closure: Gc<Closure>,    // or the native id for a native continuation frame
    pc: u32,
    base: u32,               // first register
    cell_base: u32,
    nvarargs: u32,           // extra arguments, stored just below `base`
    want: u8,                // results the caller expects (255 = all)
    kind: FrameKind,         // Lua | Native { id, k, protected } (a native waiting on CallThen)
}
```

### Dispatch

`Vm::run` is one loop over the current frame's code (ADR-4):

```rust
loop {
    let op = code[pc];
    pc += 1;
    match op {
        Op::Add { dst, a, b } => {
            let (x, y) = (regs[base + a], regs[base + b]);
            regs[base + dst] = match (x, y) {
                (Value::Int(i), Value::Int(j)) => Value::Int(i.wrapping_add(j)),
                (Value::Float(f), Value::Float(g)) => Value::Float(f + g),
                _ => self.arith_slow(ArithOp::Add, x, y, pc)?,
            };
        }
        // ...
    }
}
```

Every instruction has an inline fast path for the common types and one out-of-line `*_slow` function for everything else: `arith_slow` (mixed int/float, string coercion, errors), `index_slow`, `call_slow`, `compare_slow`, `len_slow`, `concat_slow`. **Metamethods plug into exactly these functions later**, without touching fast paths or the instruction set.

**Register window.** While a Lua frame runs, `regs` is kept at least `base + 256` long. The loop borrows `regs[base..base + 256]` as a `&mut [Value; 256]`, and register numbers are `u8`, so register access needs no bounds check and the window's address stays in a machine register (P5, ADR-4). Anything that takes `&mut self` (calls, allocation, collection steps, errors) ends the borrow, and the loop takes the window again afterwards. Values past the window, such as open-ended call results or varargs, are reached through `regs` itself. The extra slots above a frame's own registers are the callee's registers, so the only cost is a resize after a native call has shrunk the stack.

**Read registers in place.** An instruction matches on its operand registers where they are (`match (&w[a], &w[b])`, or `match w[a]`), and never copies them to a temporary `Value` first. Arithmetic writes its result as a tag byte and an 8-byte payload, two stores. A following instruction that loads the whole 16-byte value can't take it from the store buffer, and it stalls until both stores land, about 10 to 15 cycles on x86. Matching in place loads just the tag and the payload, which the store buffer forwards. On the cloud x86_64 container this cut nbody by 25% and spectral_norm by 38%.

**Hot and cold halves.** `run()` is one large function, and the compiler allocates machine registers across all of it. So an arm in `run()` holds only an instruction's common case: decoding, a type check, a few register operations. Everything slower, rarer or larger lives in `cold.rs` behind `#[inline(never)]`: allocation, cache misses, varargs, concatenation, `for` setup, error messages, and the error path itself (`Vm::raise`). New instructions follow the same split. When 26 simulated instructions were added this way (six with inline integer fast paths), instruction counts on the benchmarks moved by less than 1%. Wall time can still move by a few percent when code shifts, from alignment and branch-predictor effects, so compare changes by instruction counts (callgrind) as well as by time.

### Calls and returns (stackless, ADR-3)

- `Call{func, nargs, nresults}`: the callee is in `R[func]`, arguments above it. For a Lua closure, the new frame's `base` is `func + 1`; missing parameters are set to `nil`. For a vararg function, the extra arguments stay where they are and the frame's registers start above them, with the fixed parameters moved up; `Vararg` copies from below `base`. No Rust recursion: the loop saves `pc`, pushes the frame, and reloads `code`, `consts` and `base`.
- `Return{first, count}` copies results to where the callee was (`R[func]` of the caller), pads or truncates to the caller's `want`, pops the frame and reloads. When `want` is 255 the caller's "top" is set for the following `Call`/`Return`/`SetList` that consumes all values.
- `TailCall` replaces the current frame, so `loop(200000, 0)` runs in constant space, as Lua requires. The compiler always follows it with `Return{func, 255}`: when the callee is a native that returns `CallThen`, the VM makes an ordinary call instead and that `Return` passes the results on.
- **Fast switches.** A call to a Lua closure, and a `Return1` to a Lua caller that wants one value, switch the loop's frame state (`base`, `pc`, code, constants, caches, window) in place from values already in hand, instead of storing them in the frame and reading them back. The code is only reloaded when it changes, so a recursive call or its return never touches it. A closure calling itself skips the heap lookup for its prototype: the running frame keeps `closure` alive, so an equal handle is the same closure. Everything else (natives, errors, multiple results) goes back through the frame stack.
- **Depth limit**: pushing a frame beyond the configured limit (default one million register slots) raises the catchable error `stack overflow`. The Rust stack never grows with Lua recursion.

### Native functions

```rust
pub type NativeFn = fn(&mut NativeCtx<'_>) -> Result<NativeReturn, LuaError>;

pub enum NativeReturn {
    Return(usize),                                 // the last n values of the window are the results
    CallThen { f: Value, nargs: usize, k: Continuation, protected: bool },
}
pub type Continuation = fn(&mut NativeCtx<'_>, CallOutcome) -> Result<NativeReturn, LuaError>;
```

`NativeCtx` is the native's window of the register stack: its arguments first, then whatever it pushes. It has argument access and checks (`arg(i)`, `check_str(i)`, `check_int(i)` with Lua's `bad argument #2 to 'format' (...)` messages; the function name comes from the call site's debug info, falling back to the registered name), window editing (`set`, `push`, `truncate`), and allocation. `CallThen` calls `f` with the last `nargs` values of the window; the call's results replace those arguments, and the continuation sees the rest of the window unchanged, so state a native needs afterwards stays in its window. A native that needs to call a Lua function doesn't call back into the VM; it returns `CallThen`, the VM runs the call in its own loop, then invokes the continuation with the results or the error. That's ADR-3's continuation style, and it's how `pcall` works at first light:

```rust
fn pcall(ctx) -> ... { Ok(CallThen { f: ctx.arg(0)?, nargs: ctx.nargs() - 1, k: pcall_done, protected: true }) }
fn pcall_done(ctx, outcome) -> ... { match outcome { Ok(n) => push true + n results, Err(e) => push false, e } }
```

The same mechanism will make `pcall`, `table.sort` comparators and metamethods yieldable once coroutines arrive.

**Fast paths.** A native may also have a `FastFn = fn(&[Value]) -> Option<Value>`, set with `vm.set_native_fast_path(name, fast)`. A call that wants at most one result (or all of them) tries it first, with the arguments as a slice of the caller's registers: no window, no frame, no result copying. It returns `None` for anything it doesn't handle (wrong types, coercions, errors), and the full native then runs, so a fast path never changes behavior. It can't allocate or call Lua, which keeps it a plain function a future JIT can call directly. `math.sqrt`, `abs`, `floor`, `max` and `min` have one.

### Errors

A Lua error is `Err(LuaError { value: Value, traceback: Option<String> })` propagated with `?`, never a Rust panic. When `run` sees an error it unwinds: it pops frames until it reaches a native continuation frame marked `protected`, truncates `regs` and `cells` to that frame, and resumes it with `Err(value)`. With no protected frame left, the error returns to the embedder with a traceback built from the popped frames.

`error(msg, level)` adds `chunk:line:` for the frame at `level` (1 = the caller of `error`, 2 = its caller), taken from the line table at the frame's saved `pc`. `error()` with no value raises `nil`, which `pcall` reports and the CLI prints as `<no error object>` (5.5).

### Error messages

The reference interpreter names the culprit in type errors: `attempt to index a nil value (field 'a')`. We produce the same text from **compile-time knowledge**: when the compiler emits an instruction that can fail on an operand, it records where that operand came from (`Local("t")`, `Global("x")`, `Field("a")`, `Upvalue("u")`, `Method("m")`, `Constant("x")`) in the debug info, keyed by pc. On an error, the slow path looks up the pc and appends the description. No analysis of bytecode after the fact, and the same mechanism later gives OrbitScript better messages (it can record richer origins, such as the full expression text).

Message forms covered at first light (all checked against `lua5.5`):

- `attempt to index a nil value (local 't')`, `attempt to call a nil value (global 'f')`, `(method 'm')`, `(upvalue 'u')`, `(field 'a')`, `(constant 'x')`
- `attempt to perform arithmetic on a table value (local 's')`, `attempt to add a 'string' with a 'number'`, `attempt to concatenate a table value`, `attempt to get length of a nil value`
- `attempt to compare number with string`, `attempt to compare two table values`
- `attempt to divide by zero`, `attempt to perform 'n%0'`, `table index is nil`, `table index is NaN`
- `'for' step is zero`, `bad 'for' initial value (number expected, got string)`, `stack overflow`

### Garbage collection

- **Roots** (`impl Roots<LuaSpace> for VmRoots`): the stack's registers, cells and frame closures; the globals table; the loader's in-progress prototypes. `clear_weak` prunes the string interner.
- **Safe points**: after instructions that allocate (`NewTable`, `Closure`, `CellNew`, `Concat`, table stores that grow, native calls that allocated), the loop checks `heap.should_step()` and runs `heap.step(&mut roots)`. At a safe point every live handle is in a register, cell slot, frame or root: the loop itself holds only an `Rc<ProtoCode>` (whose constants the frame's closure keeps alive) and integers. Natives never hold handles across `CallThen`; state they need afterwards goes in their frame's registers.
- **Barriers**: table stores go through `Heap::update` (two-phase, above); `CellSet`, `CapSet` and `SetGlobal` likewise. The running stack is a root and is rescanned when marking finishes, so register writes need no barrier.
- **Memory release**: `orbit-gc`'s `malloc-trim` feature (on PR #6) returns freed pages to the OS after cycles that release them; `orbit-cli` enables it on glibc targets.
- **Stress mode**: `ORBIT_GC_STRESS=1` turns on `Heap::set_stress`, collecting at nearly every safe point; the first-light tests must pass under it.

## Standard library subset (`orbit-stdlib`)

Registered through one API (`vm.register_native("print", print)` and library tables built from `&[(&str, NativeFn)]`), with one library table per module so "open selected libraries" (plan) works from the start. Specifics:

- `print` writes through a buffered `Write` owned by the VM, flushed on exit and before an uncaught error is reported.
- `tostring` uses `orbit_core::number` and the object identity formatter (`table: 0x...`, stable per object).
- `string.format` supports `%d`, `%i`, `%s`, `%%` with width and `-`, erroring with Lua's message on a non-integral float for `%d`; everything else is `invalid conversion '%f' to 'format'` plus "not supported yet".
- `collectgarbage("count")` returns `heap.bytes() / 1024` as a float.
- `os.clock` measures monotonic time since the VM started, because stable `std` has no process-CPU-time API. The plan already lists `os.clock` as a stdlib gap to settle in stream D; scripts only use it to time themselves.

## Embedding and CLI

```rust
let mut lua = orbit::Lua::new();          // opens the first-light libraries
let f = lua.load(source, "script.lua")?;  // parse → lower → analyse → compile → link
lua.call(f, &[])?;
```

`orbit-cli`: `orbit script.lua [args...]` sets the global `arg` table (`arg[0]` is the script), runs it, and on an uncaught error prints `<program>: <message>` and a traceback to stderr with exit status 1, the shape the diff runner expects. `--dump-ast`, `--dump-hir` and `--dump-bytecode` print the intermediate forms.

## JIT readiness

A JIT (P7, an optional Cargo feature; Cranelift is the leading option) would compile hot functions from this bytecode, not from the HIR. The bytecode is where runtime type feedback lives, and HIR type annotations are optional and never enforced, so they can't be trusted for code generation. The design leaves these hooks for it:

| JIT need | Where the design provides it |
|---|---|
| Translate a function | Register bytecode maps directly onto Cranelift variables. Fused compare-and-branch instructions become native branches |
| Read and write values | `Value` has a pinned layout (above): tag byte at offset 0, 8-byte payload at offset 8, fixed tag numbers shared with table array parts |
| Specialise on types | Per-instruction type-feedback bytes (`type-feedback` feature) |
| Specialise lookups | Inline-cache slots on global and field instructions, readable as guards ("key still in cached slot") |
| Uncommon cases | The `*_slow` functions and native functions, called from JIT code as helpers through small shims in `orbit-jit` |
| Fall back to the interpreter | Frames record closure, pc, base and cell base (ADR-3). JIT code keeps Lua registers in the `regs` stack (or spills them there before any exit), so leaving JIT code is "store pc, return to the loop" |
| Captured variables | Heap cells: a load or store through the cell, with no open-upvalue bookkeeping |
| Compile off-thread | The compiler never needs the heap; a JIT takes a copy of a prototype's code, constants and feedback for a background compile, since `Rc` can't cross threads |

**The JIT–VM contract**, which `orbit-jit` (the one crate allowed `unsafe`, ADR-6) must follow:

1. **Store pc before every helper call.** Error messages, tracebacks and `error` levels all come from the frame's pc and the debug info.
2. **Reload after helpers that can move memory.** Anything that allocates can run a GC step, which can release arena pages, and a call can grow `regs` (a `Vec`, which may reallocate). After such a helper, JIT code reloads the register base and any arena page addresses. Pages are fixed-size boxed slices in `orbit-gc`, so objects never move while their page is live, and between helper calls direct loads through page addresses are valid.
3. **Handles stay handles.** A `Gc<T>` is an index, not a pointer. JIT code turns it into an address with the arena's page table (page = index >> shift). That needs `orbit-gc` to expose the page table and pin the layout of `Gc<T>` and of its slots as well, which is raised with the `orbit-gc` owner when the JIT work starts, not now.
4. **Yields leave JIT code.** Coroutine switches and native `CallThen` requests return to the interpreter loop, which handles them as it already does. JIT code never holds Rust frames across a yield.
5. **Barriers.** Stores into tables and cells from JIT code go through the same write-barrier helpers (or inline the "is the owner black?" check once `orbit-gc` exposes it).

## Performance expectations

First light is about correctness and the right structure, not speed; P5 is the tuning phase. Still, the structure is chosen so first light should land near S0's numbers (1.5 to 2x PUC Lua on `fib`, faster than PUC on allocation churn), and the PR that completes it reports `cargo testkit bench` for `fib.lua` and `binary_trees.lua`, measured on the Linux x86_64 cloud container and labelled as such.

## Implementation order

1. `orbit-core` objects, tables, strings, number formatting (needs `orbit-gc` from PR #6 merged).
2. In parallel: the front end and HIR crates (they don't need the runtime), and the compiler once the HIR types land.
3. `orbit-vm` interpreter and linker, then `orbit-stdlib`'s subset and the CLI.
4. The [end-to-end demo](README.md#end-to-end-demo) passes, including under GC stress mode.

Each step lands as its own PR with unit and golden tests.
