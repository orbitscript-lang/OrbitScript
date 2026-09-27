"""Mutated pieces of the Lua test suite, to compare syntax errors with lua5.5.

Usage: gen_mutations.py OUT SEED COUNT TESTES_DIR

Writes COUNT chunks: runs of lines from the suite files with a few token-level edits
(deletions, insertions from a vocabulary of tokens that trigger interesting errors,
swaps, copies), and whole files with one byte-level edit.
"""
import glob, os, random, re, struct, sys
random.seed(int(sys.argv[2]))
N = int(sys.argv[3])
files = sorted(glob.glob(os.path.join(sys.argv[4], '*.lua')))
srcs = []
for f in files:
    b = open(f, 'rb').read()
    if b.startswith(b'#'):
        b = b[b.index(b'\n'):]
    srcs.append(b)
vocab = [b'end', b'local', b'function', b'(', b')', b'{', b'}', b'[', b']', b'=', b'==', b',', b';', b'::', b'goto', b'break',
 b'return', b'if', b'then', b'else', b'elseif', b'do', b'while', b'repeat', b'until', b'for', b'in', b'...', b'..', b'.', b':',
 b'<const>', b'<close>', b'<', b'>', b'global', b'global *', b'*', b'x', b'y', b'_ENV', b'self', b'1', b'0x', b'3e', b'"',
 b"'", b'[[', b']]', b'[=', b'--[[', b'\\', b'\\u{', b'\\x', b'\\300', b'nil', b'and', b'or', b'not', b'#', b'~', b'@', b'$', b'\x01', b'\xff',
 b'::l::', b'goto l', b'local x <const> = 1', b'x = 2', b'global<const> *', b'global none', b'<foo>', b'1.2.3', b'0x1p', b'\n']
tok_re = re.compile(rb'[A-Za-z_][A-Za-z0-9_]*|\d+|\s+|.', re.S)
def pieces(src):
    # runs of 1 to 40 lines
    lines = src.split(b'\n')
    i = 0
    while i < len(lines):
        n = random.randint(1, 40)
        yield b'\n'.join(lines[i:i+n])
        i += n
def mutate(s):
    toks = tok_re.findall(s)
    if not toks: return s
    for _ in range(random.randint(1, 3)):
        op = random.random()
        i = random.randrange(len(toks)) if toks else 0
        if op < 0.3 and toks:
            del toks[i:i+random.randint(1, 3)]
        elif op < 0.6:
            toks.insert(i, random.choice(vocab))
        elif op < 0.75 and toks:
            j = random.randrange(len(toks)); toks[i], toks[j] = toks[j], toks[i]
        elif op < 0.85 and toks:
            toks.insert(i, random.choice(vocab) + b' ')
        elif toks:
            k = random.randrange(len(toks)); toks[i:i] = toks[k:k+random.randint(1, 8)]
    return b''.join(toks)
def mutate1(s):
    # one small mutation at a random position of a whole file
    i = random.randrange(len(s))
    op = random.random()
    if op < 0.4:
        return s[:i] + random.choice(vocab) + b' ' + s[i:]
    elif op < 0.7:
        return s[:i] + s[i+random.randint(1, 6):]
    else:
        j = random.randrange(len(s))
        return s[:i] + s[j:j+random.randint(1, 30)] + s[i:]
out = open(sys.argv[1], 'wb')
count = 0
allp = [p for s in srcs for p in pieces(s)]
while count < N:
    if random.random() < 0.25:
        p = random.choice(srcs)
        m = mutate1(p)
    else:
        p = random.choice(allp)
        m = p if random.random() < 0.05 else mutate(p)
    out.write(struct.pack('<I', len(m)) + m)
    count += 1
