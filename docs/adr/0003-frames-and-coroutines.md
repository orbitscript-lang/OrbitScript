# ADR-3: Call frames and coroutines

Status: Proposed (implemented in `orbit-vm`; the native API below is what stream D builds on)

## Context
Lua lets code yield from inside `pcall`, metamethods, iterators and native functions, and scripts can recurse deeply. A VM that maps Lua calls onto Rust calls makes both hard: the Rust stack can overflow, and Rust frames can't be suspended.

## Decision
- **Stackless interpreter.** A Lua-to-Lua call pushes a frame on the coroutine's value stack and continues in the same dispatch loop; it never recurses on the Rust stack. Metamethods written in Lua run the same way: the interrupted instruction is completed when the metamethod's frame returns (`luaV_finishOp`).
- Each coroutine owns its value stack and frame list. The running coroutine's state lives in the `Vm`; switching moves it into the coroutine's `Thread` heap object (stack and open upvalues, which the collector traces) plus a private saved state with no GC handles, and moves the target's state out. Frames hold no GC handles: a frame's function is in its stack slot.
- **Native functions** are `fn(&mut Vm) -> LuaResult<Action>`. They see their arguments as a 1-based stack window (`arg`, `get`, `set`, `push`, `pop`, `insert`, `remove`, `top`, `set_top`) and return an `Action`:
  - `Return(n)`: return the top `n` values.
  - `Call { nargs, nresults, k, ctx }`: call the function below the top `nargs` values, then run continuation `k(vm, status, ctx)` with the results on the stack (`lua_callk`).
  - `PCall { nargs, nresults, handler, k, ctx }`: the same, catching errors; `k` gets `KStatus::Error(status)` with the error object on top after the message handler ran and `<close>` variables were closed (`lua_pcallk`).
  - `Yield { n, k }`: yield the top `n` values; on resume the resume arguments become the results, or go to `k` (`lua_yieldk`).

  Because the callee runs in the dispatch loop while the native function's frame waits, the callee may yield, which makes `pcall`, `dofile`, `pairs`' `__pairs` and future library callbacks (`table.sort`, `string.gsub`) yieldable. For simple cases `Vm::call` and `Vm::pcall` also exist; they run the callee to completion on the Rust stack and make it non-yieldable ("attempt to yield across a C-call boundary"), as `lua_call` without a continuation does.
- Errors are `Result` values (`Error { value, status }`), never Rust panics. They unwind to the nearest native frame waiting on a `PCall` within the current run of the loop, whose message handler runs with the failing frames still in place (for tracebacks), then `<close>` handlers run and the continuation resumes.
- `coroutine.resume` recurses on the Rust stack, as the reference does, bounded by the same 200-level limit as re-entrant calls ("C stack overflow").
- The frame layout records enough (function slot, pc, base, expected results) for the JIT to deoptimize back into the interpreter later.

## Consequences
- Some native library functions are written as small state machines rather than straight-line code (see `pcall`/`xpcall` in `orbit-vm/src/baselib.rs`).
- Lua recursion depth is limited by the stack size (`MAX_STACK`, one million slots), not the Rust stack, and produces the "stack overflow" error the suite checks. Rust recursion only comes from re-entrant calls, `resume` and metamethods called from native code, all counted against the 200-level limit.
- Only the running coroutine's stack is in `Vm` registers; suspended coroutines' stacks are ordinary heap data, so upvalues pointing into them go through the `Thread` object.
