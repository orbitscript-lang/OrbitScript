# ADR-7: Dependency policy

Status: Accepted

## Context

Every dependency adds build time, binary size, supply-chain risk and a license to track. The core of the runtime should stay small and auditable.

## Decision

- `orbit-gc`, `orbit-runtime` and `orbit-compiler` take no runtime dependencies without an ADR.
- Every runtime dependency gets a row in `DEPS.md` in the PR that adds it: what uses it, why, the alternatives considered, and the measured change in release binary size.
- Prefer established, focused crates with permissive licenses (MIT, Apache-2.0, BSD, CC0, Zlib) over hand-written replacements, and few of them.
- Build-time dependencies (proc macros, `cc`) are fine when nothing from them is linked in; they are listed too.
- Dev-dependencies are listed but held to a lighter bar, since they don't ship.

### Platform gaps

Where `std` can't do something the platform can (local time, CPU time, terminal modes, the C library's error text), the gap is filled outside the core crates, in `orbit-std` or the program that needs it.

- **A gap is filled in one of three ways:** safe Rust of our own, a crate with a safe interface, or a narrow `unsafe` module on ADR-6's exception list.
- **Never by running another program, and never by depending on an undocumented output format,** ours or another program's (a message text `std` doesn't promise, another tool's output). Running a program satisfies the rules on `unsafe` and dependencies to the letter, but adds a process boundary, a `PATH` lookup and a text protocol to what is one system call, and fails quietly where the program is missing. An undocumented format can change under us with no notice.
- **The pull request that fills a gap records the options it compared,** as `DEPS.md` does for a dependency.

How to choose, correctness first:

1. **Does data a script controls reach the mechanism?** Memory safety is the sandbox boundary, so prefer safe Rust. Where the behaviour to match is the C library's own, call it, but check the script's data in safe Rust first and pass C only numbers and pieces already checked: `os.date` validates every conversion of a format and hands `strftime` one at a time, and the `struct tm` it formats always comes from the C library.
2. **Is it a few fixed system calls with no script data?** Prefer an established crate with a safe interface that builds on the minimum supported Rust. The line editor's terminal modes are this case (`rustix`).
3. **Is it one or two calls into something already linked, the C library say?** An ADR-6 exception module, kept as narrow as the calls. The C library's error text (`strerror`) and local time are this case.

## Consequences

- The core crates build with no third-party code at run time.
- `DEPS.md` is the single list of what ships and what it costs.
