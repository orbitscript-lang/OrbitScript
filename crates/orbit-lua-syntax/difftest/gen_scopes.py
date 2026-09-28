"""Random programs built from scoping constructs, to compare static errors with lua5.5.

Usage: gen_scopes.py OUT SEED COUNT

The programs mix locals with attributes, global declarations, labels and gotos, loops,
nested functions and varargs, so most of them hit a static check: const assignment,
undeclared globals, goto scoping, break placement, attribute rules.
"""
import random, struct, sys
random.seed(int(sys.argv[2])); N = int(sys.argv[3])
names = ['a', 'b', 'x', '_ENV', 'print', 'f']
labels = ['l1', 'l2', 'continue']
def expr(d):
    r = random.random()
    if d > 2 or r < 0.3: return random.choice(['1', '2.5', '"s"', 'nil', 'true', random.choice(names), '...', '-0.0', '1//0', '3&1.5', '2^2', 'not nil'])
    if r < 0.5: return expr(d+1) + ' ' + random.choice(['+', '..', 'and', 'or', '==', '//', '&', '<<']) + ' ' + expr(d+1)
    if r < 0.6: return 'function(' + random.choice(['', 'a', 'a, ...', '...t', '...']) + ') ' + block(d+1) + ' end'
    if r < 0.7: return random.choice(names) + '(' + expr(d+1) + ')'
    if r < 0.8: return '{' + expr(d+1) + ', k = ' + expr(d+1) + '}'
    return '(' + expr(d+1) + ')'
def attrib():
    return random.choice(['', '', ' <const>', ' <close>', ' <foo>'])
def stat(d):
    r = random.random()
    n = random.choice(names)
    if r < 0.12: return 'local ' + random.choice(['', '<const> ']) + n + attrib() + random.choice(['', ', ' + random.choice(names) + attrib()]) + random.choice(['', ' = ' + expr(d), ' = ' + expr(d) + ', ' + expr(d)])
    if r < 0.2: return n + random.choice([' = ', ', ' + random.choice(names) + ' = ']) + expr(d)
    if r < 0.28: return 'goto ' + random.choice(labels)
    if r < 0.36: return '::' + random.choice(labels) + '::'
    if r < 0.42: return 'global ' + random.choice(['*', '<const> *', n, n + ' <const>', '<const> ' + n, n + ' = ' + expr(d), n + ' <close>', 'function ' + n + '() ' + block(d+1) + ' end'])
    if d > 3: return 'break'
    if r < 0.5: return 'do ' + block(d+1) + ' end'
    if r < 0.56: return 'while ' + expr(d) + ' do ' + block(d+1) + ' end'
    if r < 0.62: return 'repeat ' + block(d+1) + ' until ' + expr(d)
    if r < 0.68: return 'for ' + n + ' = 1, 2 do ' + block(d+1) + ' end'
    if r < 0.72: return 'for ' + n + ', ' + random.choice(names) + ' in ' + expr(d) + ' do ' + block(d+1) + ' end'
    if r < 0.78: return 'local function ' + n + '(' + random.choice(['', 'self', '...', 'a, ...b']) + ') ' + block(d+1) + ' end'
    if r < 0.84: return 'function ' + n + random.choice(['', '.y', ':m']) + '() ' + block(d+1) + ' end'
    if r < 0.88: return 'if ' + expr(d) + ' then ' + block(d+1) + ' else ' + block(d+1) + ' end'
    if r < 0.92: return 'break'
    if r < 0.96: return ';'
    return random.choice(names) + '(' + expr(d) + ')'
def block(d):
    k = random.randint(0, 4 if d < 3 else 2)
    s = [stat(d) for _ in range(k)]
    if random.random() < 0.1: s.append('return ' + expr(d))
    return random.choice([' ', '\n', ' ; ']).join(s)
out = open(sys.argv[1], 'wb')
for _ in range(N):
    p = block(0).encode()
    out.write(struct.pack('<I', len(p)) + p)
