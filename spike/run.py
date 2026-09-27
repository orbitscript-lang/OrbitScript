#!/usr/bin/env python3
"""Run one command REPEATS times via target/measure (a small C fork/exec wrapper).

Prints: best wall ms, peak RSS KiB (max over runs), and the program's last stdout line.
usage: run.py REPEATS cmd args...
(Builds target/measure from measure.c on first use.)
"""
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
MEASURE = os.path.join(HERE, "target", "measure")
if not os.path.exists(MEASURE):
    os.makedirs(os.path.dirname(MEASURE), exist_ok=True)
    subprocess.run(["cc", "-O2", "-o", MEASURE, os.path.join(HERE, "measure.c")], check=True)
reps = int(sys.argv[1]); cmd = sys.argv[2:]
best, rss, out = None, 0, None
for _ in range(reps):
    p = subprocess.run([MEASURE] + cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"failed: {cmd}\n{p.stderr}")
    m = [l for l in p.stderr.splitlines() if l.startswith("MEASURE ")][-1].split()
    ms, kb = float(m[1]), int(m[2])
    best = ms if best is None else min(best, ms)
    rss = max(rss, kb)
    out = p.stdout.strip().splitlines()[-1]
print(f"{best:.0f} {rss} {out}")
