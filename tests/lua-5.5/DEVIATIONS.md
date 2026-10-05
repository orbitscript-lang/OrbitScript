# Deliberate differences from lua5.5

OrbitScript aims to behave like the reference `lua5.5` (5.5.1) wherever a program can observe it. This file lists the places where it doesn't, on purpose. Each entry says what differs, why, and whether any suite file or `tests/diff` program depends on it. Every entry was flagged to Tym in the PR that added it.

Bugs are not deviations: a difference that isn't listed here is a bug.

## `os.clock` is wall time

`os.clock()` returns the seconds of wall-clock time since the VM started, where lua5.5 returns the CPU time the process has used. The two agree for a busy single-threaded program and differ when the program sleeps, waits on I/O or runs a child process (`os.clock` keeps counting here and not in lua5.5).

Why: Rust's standard library has no portable CPU-time clock, and Tym chose to add no dependency for it (ADR-7 decision, 2026-10-05).

Suite: none depends on it. The suite only checks that `os.clock` returns a number that doesn't go backwards.

## Local time on Windows is UTC

On Windows, `os.date` without `!`, `os.date("*t")` and `os.time{...}` use UTC, because our zone reader reads the `TZ` variable and the TZif database, which Windows doesn't have. On Linux and macOS local time matches lua5.5.

Why: same ADR-7 decision; a Windows zone source would need a dependency.

Suite: none depends on it (the suite runs on Unix).
