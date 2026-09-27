"""Chunks declaring `local vzq <const> = E` for random constant-ish expressions E.

Usage: gen_consts.py OUT SEED COUNT

Compares which declarations the parser and lua5.5 treat as compile-time constants,
which follows the reference compiler's folding and jump-list rules.
"""
import random, struct, sys
random.seed(int(sys.argv[2]))
atoms = ['1', '0', '2.5', '0.0', '"s"', 'nil', 'true', 'false', 'c', 'k', 'x', '1e308', '-1', '3', '0x7fffffffffffffff', '2^53', '(1)', '...']
ops = ['+', '-', '*', '/', '//', '%', '^', '&', '|', '~', '<<', '>>', '..', '==', '<', 'and', 'or']
def e(d):
    r = random.random()
    if d > 3 or r < 0.3: return random.choice(atoms)
    if r < 0.45: return random.choice(['-', 'not ', '~', '#']) + e(d+1)
    if r < 0.55: return '(' + e(d+1) + ')'
    return e(d+1) + ' ' + random.choice(ops) + ' ' + e(d+1)
out = open(sys.argv[1], 'wb')
for _ in range(int(sys.argv[3])):
    p = ('local c <const> = 10; local k <const> = "str"; local x = 1; local vzq <const> = ' + e(0) + '; local marker = 1').encode()
    out.write(struct.pack('<I', len(p)) + p)
