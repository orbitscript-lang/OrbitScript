#!/usr/bin/env python3
"""Run every allocator variant on every workload and print medians.

Usage: python3 alloc/run.py TARGET_ROOT [reps] [variant substring]
TARGET_ROOT holds one Cargo target dir per build variant (see README.md).
"""
import os, statistics, subprocess, sys

root = sys.argv[1]
reps = int(sys.argv[2]) if len(sys.argv) > 2 else 5

# name, build dir, extra environment
VARIANTS = [
    ("glibc", "system", {}),
    ("glibc + malloc_trim", "trim", {}),
    ("mimalloc 3", "mi3", {}),
    ("mimalloc 3, purge_delay=0", "mi3", {"MIMALLOC_PURGE_DELAY": "0"}),
    ("mimalloc 3 + mi_collect", "mi3c", {}),
    ("mimalloc 2", "mi2", {}),
    ("mimalloc 2, purge_delay=0", "mi2", {"MIMALLOC_PURGE_DELAY": "0"}),
    ("musl", "musl-system", {}),
    ("musl + mimalloc 3", "musl-mi3", {}),
    ("musl + mimalloc 3, purge_delay=0", "musl-mi3", {"MIMALLOC_PURGE_DELAY": "0"}),
    ("musl + mimalloc 2", "musl-mi2", {}),
]
if len(sys.argv) > 3:
    VARIANTS = [v for v in VARIANTS if sys.argv[3] in v[0]]
WORKLOADS = {
    "trees": (["trees", "16"], ["secs", "peak_rss_mib", "longest_step_ms"]),
    "churn": (["churn", "5000000"], ["secs", "peak_rss_mib", "longest_step_ms"]),
    "spike": (["spike"], ["rest_before_mib", "r1_build_s", "r2_build_s", "peak_rss_mib",
                          "r1_rest_mib", "r3_rest_mib", "r3_1s_mib"]),
}

def run(build, env, args):
    out = subprocess.run([f"{root}/{build}/release/spike-alloc", *args], env={**os.environ, **env},
                         capture_output=True, text=True, check=True).stdout
    return dict(kv.split("=", 1) for kv in out.split())

for wname, (args, keys) in WORKLOADS.items():
    print(f"\n## {wname}\n")
    print("| variant | " + " | ".join(keys) + " |")
    print("|---|" + "---|" * len(keys))
    for name, build, env in VARIANTS:
        runs = [run(build, env, args) for _ in range(reps)]
        cells = [f"{statistics.median(float(r[k]) for r in runs):.3g}" for k in keys]
        print(f"| {name} | " + " | ".join(cells) + " |")
