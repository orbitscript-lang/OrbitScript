#!/usr/bin/env python3
"""check-handlers.py BINARY SUBSTR (for the interpreter: 8handlers): functions whose entry saves registers or makes a stack frame, and their non-tail calls."""
import sys, subprocess, re
out = subprocess.run(['objdump', '-d', '--no-show-raw-insn', sys.argv[1]], capture_output=True, text=True).stdout
name = None; rows = {}
for line in out.splitlines():
    m = re.match(r'^[0-9a-f]+ <(.*)>:$', line)
    if m:
        name = m.group(1); rows[name] = []; continue
    if name and '\t' in line: rows[name].append(line.split('\t')[-1].strip())
bad = 0; n = 0
for k, ins in rows.items():
    if sys.argv[2] not in k or not ins: continue
    short = re.sub(r'^.*?(\d+)([a-z_0-9]+)$', r'\2', k)[-28:]
    n += 1
    pushes = 0
    for x in ins[:8]:
        if x.startswith('push') or x.startswith('sub') and '%rsp' in x: pushes += 1
        else: break
    calls = [x for x in ins if x.startswith('call')]
    jmps = len([x for x in ins if x.startswith('jmp') and ('*' in x or '<' in x and '+0x' not in x)])
    if pushes or calls:
        bad += 1
        print(f'{short:30} entry saves={pushes} calls={len(calls)} tailjumps={jmps} len={len(ins)}  ' + '; '.join(sorted(set(re.sub(r'.*<(.*)>', r'\1', c)[-30:] for c in calls)))[:150])
print(f'{bad} of {n} functions have a prologue or a call')
