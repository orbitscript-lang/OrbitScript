#!/usr/bin/env python3
"""Run the S0 benchmark matrix and print Markdown tables.

usage: bench.py LUA [REPEATS]
LUA is a reference lua5.5 binary built from lua/lua with -O2.
"""
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
LUA = sys.argv[1]
REPS = sys.argv[2] if len(sys.argv) > 2 else "5"
SPIKE = os.path.join(HERE, "target", "release", "spike")
SIZES = {"fib": 35, "loop": 3_000_000, "str": 1_000_000, "gc": 3_000_000}
OBJS = ["objs_tables", "objs_closures", "objs_strings"]
CONFIGS = [
    ("PUC Lua 5.5", None),
    ("arena, match", ("arena", "match")),
    ("arena, super", ("arena", "super")),
    ("arena, closure", ("arena", "closure")),
    ("gc-arena, match", ("gcarena", "match")),
    ("gc-arena, super", ("gcarena", "super")),
    ("boa_gc, match", ("boagc", "match")),
    ("boa_gc, super", ("boagc", "super")),
]

def run(cfg, bench, n, reps=REPS):
    if cfg is None:
        cmd = [LUA, os.path.join(HERE, "lua", bench + ".lua"), str(n)]
    else:
        cmd = [SPIKE, cfg[0], cfg[1], bench, str(n)]
    out = subprocess.run([os.path.join(HERE, "run.py"), reps] + cmd,
                         capture_output=True, text=True, check=True).stdout.split()
    return float(out[0]), int(out[1]), out[2]

print("## Speed (best of %s runs, wall ms; ratio to PUC Lua; peak RSS MiB)\n" % REPS)
print("| config | " + " | ".join(f"{b} ({n:,})" for b, n in SIZES.items()) + " |")
print("|---|" + "---|" * len(SIZES))
base = {}
for name, cfg in CONFIGS:
    cells = []
    for b, n in SIZES.items():
        ms, kb, res = run(cfg, b, n)
        if cfg is None:
            base[b] = (ms, res)
        elif res != base[b][1]:
            sys.exit(f"{name} {b}: result {res} != lua {base[b][1]}")
        ratio = ms / base[b][0]
        cells.append(f"{ms:.0f} ({ratio:.2f}x), {kb/1024:.0f}")
    print(f"| {name} | " + " | ".join(cells) + " |")

N = 1_000_000
print(f"\n## Bytes per object (peak RSS delta / N, N = {N:,}; includes the holding array's slot)\n")
print("| config | " + " | ".join(OBJS) + " |")
print("|---|" + "---|" * len(OBJS))
for name, cfg in [c for c in CONFIGS if c[1] is None or c[1][1] == "match"]:
    cells = []
    for b in OBJS:
        _, kb1, _ = run(cfg, b, 1, "1")
        _, kbn, _ = run(cfg, b, N, "1")
        cells.append(f"{(kbn - kb1) * 1024 / N:.0f}")
    print(f"| {name} | " + " | ".join(cells) + " |")
