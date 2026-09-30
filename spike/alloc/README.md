# Spike: mimalloc vs the system allocator

Throwaway code, asked for by Tym on 2026-09-30: what integrating mimalloc would look like, and how it compares with the system allocator, especially for giving memory back after an allocation spike (the open question from orbit-gc's page release).

## What integration looks like

For a binary (the `orbit` CLI), it is one dependency and two lines, with no `unsafe`:

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

- **Dependencies:** `mimalloc` 0.1.52 (MIT) and `libmimalloc-sys`, which compiles mimalloc's C source with the `cc` build dependency. Building needs a C compiler for the target, so cross-builds for embedded Linux need `CC_<target>` pointing at the SDK's compiler. Default is mimalloc 3.x; the `v2` feature selects 2.x.
- **Size:** +145 KiB release binary with 3.x, +124 KiB with 2.x (glibc build 384 KiB).
- **Libraries stay neutral:** the global allocator is the final binary's choice, so `orbit-gc`, `orbit-core` and `orbit` never depend on it. Embedders pick their own.
- **Tuning needs FFI or the environment:** mimalloc's options (`purge_delay` below) are set with `MIMALLOC_*` environment variables or `mi_option_set`, a C call, which is one `unsafe` line, like glibc's `malloc_trim`.

## Method

`src/main.rs` drives orbit-gc (PR #6, default `Params`) with three workloads. Each node owns a `String`, so the allocator sees small objects as well as orbit-gc's 64 to 128 KiB pages:

- **trees**: binary trees, depth 16 (steady churn, like binary_trees.lua).
- **churn**: 50,000 live objects replaced at random 5 million times, strings of 8 to 400 bytes plus occasional 8 KB ones (a long-running process: does RSS creep?).
- **spike**: a 3 MiB resting heap, then about 200 MiB allocated and dropped, three times. After two full collections the GC heap is back to 1 MiB and orbit-gc has released all but 11 pages; the table shows what the **process** still holds (`VmRSS`), right away and after 1 s idle.

Five runs each, medians, x86_64, 4 cores, Rust 1.95, glibc 2.39, and the musl that Rust's `x86_64-unknown-linux-musl` target bundles. Variants: glibc as is; glibc plus `malloc_trim(0)` after full collections (`--features trim`); mimalloc 3 and 2, as is, with `MIMALLOC_PURGE_DELAY=0`, or with `mi_collect(true)` after full collections (`--features mi-collect`). Reproduce:

```sh
cd spike
b() { CARGO_TARGET_DIR=/tmp/alloc/$1 cargo build --release -p spike-alloc "${@:2}"; }
b system; b trim --features trim; b mi3 --features mimalloc
b mi2 --features mimalloc-v2; b mi3c --features mi-collect
# musl builds: add --target x86_64-unknown-linux-musl (needs musl-gcc for mimalloc),
# then copy the binary to /tmp/alloc/musl-*/release/.
python3 alloc/run.py /tmp/alloc 5
```

## Results: glibc target

### trees

| variant | secs | peak_rss_mib | longest_step_ms |
|---|---|---|---|
| glibc | 1.35 | 55.1 | 0.49 |
| glibc + malloc_trim | 1.32 | 55.2 | 0.49 |
| mimalloc 3 | 1.18 | 70.5 | 7.21 |
| mimalloc 3, purge_delay=0 | 1.3 | 62.3 | 0.56 |
| mimalloc 3 + mi_collect | 1.15 | 76.1 | 6.26 |
| mimalloc 2 | 1.04 | 60.2 | 0.43 |
| mimalloc 2, purge_delay=0 | 1.42 | 51.7 | 0.42 |

### churn

| variant | secs | peak_rss_mib | longest_step_ms |
|---|---|---|---|
| glibc | 1.44 | 46.9 | 0.76 |
| glibc + malloc_trim | 1.38 | 46.9 | 0.72 |
| mimalloc 3 | 0.719 | 62.3 | 0.48 |
| mimalloc 3, purge_delay=0 | 0.736 | 58.9 | 0.57 |
| mimalloc 3 + mi_collect | 0.738 | 62.2 | 0.52 |
| mimalloc 2 | 0.73 | 54.2 | 0.6 |
| mimalloc 2, purge_delay=0 | 0.851 | 53.8 | 0.6 |

### spike

| variant | rest_before_mib | r1_build_s | r2_build_s | peak_rss_mib | r1_rest_mib | r3_rest_mib | r3_1s_mib |
|---|---|---|---|---|---|---|---|
| glibc | 2.8 | 0.141 | 0.071 | 202 | 200 | 200 | 200 |
| glibc + malloc_trim | 2.8 | 0.195 | 0.14 | 202 | 3 | 3.1 | 4.2 |
| mimalloc 3 | 10.3 | 0.152 | 0.104 | 223 | 222 | 223 | 223 |
| mimalloc 3, purge_delay=0 | 10.3 | 0.711 | 0.171 | 222 | 21.6 | 17.8 | 18 |
| mimalloc 3 + mi_collect | 10.1 | 0.684 | 0.412 | 224 | 14.8 | 14.8 | 15 |
| mimalloc 2 | 4.2 | 0.148 | 0.09 | 226 | 226 | 226 | 226 |
| mimalloc 2, purge_delay=0 | 4.3 | 0.648 | 0.597 | 226 | 4.9 | 5 | 5.8 |

## Results: musl target (common on embedded Linux)


### trees

| variant | secs | peak_rss_mib | longest_step_ms |
|---|---|---|---|
| musl | 2.72 | 47.5 | 0.46 |
| musl + mimalloc 3 | 2.48 | 74.5 | 7.02 |
| musl + mimalloc 3, purge_delay=0 | 2.57 | 60.6 | 6.73 |
| musl + mimalloc 2 | 2.35 | 58.4 | 0.49 |

### churn

| variant | secs | peak_rss_mib | longest_step_ms |
|---|---|---|---|
| musl | 2.58 | 49.6 | 0.73 |
| musl + mimalloc 3 | 2.06 | 58.5 | 0.66 |
| musl + mimalloc 3, purge_delay=0 | 2.19 | 57.2 | 0.67 |
| musl + mimalloc 2 | 2.08 | 52.4 | 0.66 |

### spike

| variant | rest_before_mib | r1_build_s | r2_build_s | peak_rss_mib | r1_rest_mib | r3_rest_mib | r3_1s_mib |
|---|---|---|---|---|---|---|---|
| musl | 1.1 | 0.299 | 0.298 | 217 | 2.1 | 2.1 | 2.9 |
| musl + mimalloc 3 | 8.6 | 0.302 | 0.237 | 222 | 221 | 222 | 222 |
| musl + mimalloc 3, purge_delay=0 | 8.6 | 0.809 | 0.259 | 220 | 19.9 | 16.1 | 16.2 |
| musl + mimalloc 2 | 2.5 | 0.245 | 0.211 | 224 | 224 | 224 | 224 |

## Findings

1. **Speed:** mimalloc is faster on allocation-heavy work: churn runs in half the time (0.72 s against 1.44 s), trees 12 to 23% faster (mimalloc 2 is the fastest here).
2. **Peak memory:** mimalloc holds more: +10 to 38% peak RSS on trees, +15 to 33% on churn. Only mimalloc 2 with `purge_delay=0` comes in under glibc (51.7 against 55.1 MiB on trees), and it gives up its speed on trees to do it.
3. **Giving memory back: neither allocator does it by default.** After the spike, glibc keeps 200 MiB and mimalloc keeps 222 to 226 MiB, even after 1 s idle and further activity. mimalloc only purges from inside its own allocation calls, after a delay, and in this workload the calls that come later never trigger it. The fixes:
   - glibc + `malloc_trim(0)`: back to 3 MiB, with no measurable cost on trees or churn.
   - mimalloc `purge_delay=0`: back to 5 MiB (2.x) or 18 MiB (3.x); trees slows down to 1.30 to 1.42 s, about glibc's speed.
   - mimalloc + `mi_collect(true)`: back to 15 MiB (3.x).
   - A fixed glibc mmap threshold (`GLIBC_TUNABLES=glibc.malloc.mmap_threshold=65536`, spot check) only returned 20 of 200 MiB: the small strings live in the main heap, which only `malloc_trim` gives back.
4. **Pauses:** mimalloc 3 now and then takes a 6 to 7 ms step on trees (median of five runs; single runs vary from 0.4 to 7 ms), against under 0.6 ms for glibc and mimalloc 2. Its system time is higher too (0.25 s against 0.04 s), so this looks like OS page commits and decommits landing inside a step. Not investigated further.
5. **musl:** its malloc gives memory back on its own (2 MiB after the spike) and has the lowest peak, but everything runs about twice as slowly as on glibc. mimalloc recovers only 10 to 20% of that, so the allocator is not most of the gap (not investigated).
6. **Cost of using mimalloc:** a C build step (a C cross-compiler for embedded targets), +124 to 145 KiB, and still one FFI call or an environment variable to give memory back.

## Recommendation

- The `orbit` CLI on glibc: keep the system allocator and call `malloc_trim(0)` after a full collection that released pages. It is the smallest change, adds no dependency, has the lowest peak RSS, and is the only default setup that returns a spike (to 3 MiB).
- mimalloc as an opt-in `mimalloc` feature of the CLI, for users who care about throughput more than resident memory: 2.x, with `purge_delay` set to 0 at startup. That also needs one FFI call.
- Libraries (`orbit-gc`, `orbit-core`, `orbit`) stay allocator-neutral, and the docs tell embedders about `malloc_trim` and `MIMALLOC_PURGE_DELAY`.
