//! The `coroutine` library ([VM semantics]). The VM does the switching;
//! these natives check arguments and shape results.
//!
//! [VM semantics]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use crate::stack::{Arg, FunctionArg, LibraryRow, Stack, StackResult, ThreadRef, library_row};
use crate::vm::natives::coroutine as natives;
use orbit_macros::native;

// `resume`, `yield`, `wrap` and `close` hand control to the VM, which
// switches coroutines: they are raw natives in `vm::natives::coroutine`.
pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<create>(),
    LibraryRow::raw("coroutine.resume", natives::resume),
    LibraryRow::raw("coroutine.yield", natives::yield_),
    library_row::<status>(),
    library_row::<running>(),
    library_row::<isyieldable>(),
    LibraryRow::raw("coroutine.wrap", natives::wrap),
    LibraryRow::raw("coroutine.close", natives::close),
];

#[native(crate = crate, name = "coroutine.create")]
fn create(s: &mut Stack<'_>, f: FunctionArg) -> ThreadRef {
    s.push_new_coroutine(f.0)
}

#[native(crate = crate, name = "coroutine.status")]
fn status(s: &mut Stack<'_>, co: ThreadRef) -> &'static str {
    s.thread_status(co).name()
}

#[native(crate = crate, name = "coroutine.running")]
fn running(s: &mut Stack<'_>) -> (ThreadRef, bool) {
    let t = s.running_thread();
    (t, t == s.main_thread())
}

// With no argument, the running coroutine; a nil argument is checked as a
// thread, as the reference does.
#[native(crate = crate, name = "coroutine.isyieldable")]
fn isyieldable(s: &mut Stack<'_>) -> StackResult<bool> {
    let t = if s.top() == 0 {
        s.running_thread()
    } else {
        <ThreadRef as Arg>::check(s, 1)?
    };
    Ok(s.thread_yieldable(t))
}
