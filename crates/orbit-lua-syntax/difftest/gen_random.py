"""Random byte strings and symbol soup, to check that the parser never panics and
agrees with lua5.5 on garbage.

Usage: gen_random.py OUT SEED COUNT
"""
import random, struct, sys
random.seed(int(sys.argv[2]))
alpha = b'abcxyz_019.eEpPxX+-*/%^#&~|<>=(){}[];:,"\'\\\n\r\t [==[]]--global<const>*...'
out = open(sys.argv[1], 'wb')
for _ in range(int(sys.argv[3])):
    n = random.randint(0, 40)
    if random.random() < 0.5:
        b = bytes(random.randrange(256) for _ in range(n))
    else:
        b = bytes(random.choice(alpha) for _ in range(n))
    out.write(struct.pack('<I', len(b)) + b)
