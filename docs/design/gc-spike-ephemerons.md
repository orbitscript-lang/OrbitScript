# Spike: ephemerons and finalization in orbit-gc

Status: Done (informed ADR-2's amendment "Lua weak tables and finalizers", accepted 2026-10-07).

The GC semantics design asked for two API alternatives to be measured before ADR-2 is amended: how ephemerons are marked, and how unreachable objects registered for `__gc` are found. The code is on branch `spike/gc-ephemerons`: the alternatives sit behind spike-only Cargo features of `orbit-gc`, and `crates/orbit-gc/examples/ephemeron_spike.rs` runs the workloads.

## Alternatives

| | What it does |
|---|---|
| **E1** (current) | An ephemeron value waits on its key in a hash map keyed by the key's slot; marking the key releases the value. Each entry is handled once per cycle. |
| **E1 tuned** | E1 with a cheap slot hasher instead of SipHash and the first waiting value stored inline instead of in a `Vec`. |
| **E2** | Rescan: ephemeron tables whose keys are not all marked are revisited at the end of marking until a pass marks nothing new. |
| **F1** (current) | A list of registered objects; at the end of marking, unreachable ones move to the finalization queue. |
| **F3** | No list: scan the color bytes for objects with the FIN bit at the end of marking, ordered by a per-slot registration number. |

## Workloads

- **a1**: one ephemeron table of n entries; keys held by a root array traced before the table; values are leaves.
- **a2**: the same, with the table traced before the keys, so every value waits.
- **b**: the table's keys are all dead.
- **c-chain**: value i holds key i+1; only key 0 is rooted. The worst case for rescanning. **c-many8** deals the chain over tables of 8 entries.
- **d**: binary trees with no ephemerons (overhead when unused).
- **fin**: registration cost; atomic pause with 1,000 or 1,000,000 of 1M objects registered and alive; churn (1M registered, 10% replaced each cycle).

## Results

Cloud container (x86_64 Ubuntu 24.04, 4 cores), release, pinned to one core, median of 5 runs. Time of a full collection in milliseconds (the atomic pause of an incremental cycle behaves the same).

| Workload | n | E1 | E1 tuned | E2 |
|---|---|---|---|---|
| a1 keys first | 1M | 16.0 | 19.9 | 20.5 |
| a2 table first | 100k | 17.2 | 8.2 | 1.8 |
| a2 table first | 1M | 404 | 209 | 17.9 |
| b dead keys | 1M | 380 | 129 | 13.0 |
| c-chain | 10k | 1.3 | 0.47 | 102 |
| c-chain | 100k | 14.5 | 6.0 | 9,496 |
| c-many8 | 100k | 15.4 | 6.4 | 12,682 |
| d trees (no ephemerons) | 2M | 29.0 | 32.9 | 31.3 |

Wait-map memory at 1M waiting entries: 92.6 MB for E1, 60.6 MB tuned, none for E2.

| Finalizers (1M objects) | F1 | F3 |
|---|---|---|
| register, ns per object | 5.2 | 4.6 |
| registry bytes per object | 8.4 | 4.0 |
| atomic pause, 1,000 registered | 0.01 ms | 0.09 ms |
| atomic pause, 1M registered | 1.02 ms | 0.45 ms |
| churn cycle (10% die) | 18.0 ms | 20.7 ms |

## Findings

- **E2 is much faster on caches and catastrophic on chains.** Its rescans are quadratic in chain length: 9.5 s for 100,000 links, where E1 takes 15 ms. A program can build such a chain by accident (memoizing on objects whose results reference other memoized objects), and for long-running embedded processes a bounded worst case matters more than the best case. **Keep E1.**
- **E1 tuned** roughly halves E1's time and cuts the wait map by a third on the workloads that wait, at no cost elsewhere (a1 and d are within noise). No dependency is added. **Adopted** after the amendment was approved: `orbit-gc` uses it since the "faster ephemeron wait map" PR.
- **F3 saves memory only when many objects are registered**, and pays for every heap object each cycle even when few are (9 times F1's pause with 1,000 registered), and more under churn. Lua programs register few objects. **Keep F1.**
- Neither alternative changes the API embedders see, so the ADR-2 amendment fixes the semantics and leaves these as implementation choices.
