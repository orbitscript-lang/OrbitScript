-- Lua 5.5 named varargs: the vararg table, its n field, and ... reflecting it.
local function f(...t) return t.n, t[1], t[2], t[3], t[0], t[-1], t.x, t[1.0] end
print(f())
print(f(1, nil, 3))
local function g(a, ...t) return a, t.n, ... end
print(g(1, 2, 3))
local function w(...t) t[1] = 10; return ... end
print(w(1, 2))
local function n(...t) t.n = 1; return ... end
print(n(1, 2, 3))
local function grow(...t) t.n = 4; t[4] = "four" return ... end
print(grow(1))
local function esc(...t) return t end
local tt = esc(5, 6)
print(type(tt), tt.n, tt[1], tt[2])
local function up(...t) return function() return t.n, t[1] end end
print(up("a", "b")())
local function count(...t) local u = t; u.n = 0; return select("#", ...) end
print(count(1, 2, 3))
local function bad(...t) t.n = "x" return ... end
print(pcall(bad, 1))
local function badf(...t) t.n = 1.0 return ... end
print(pcall(badf, 1))
local function neg(...t) t.n = -1 return ... end
print(pcall(neg, 1))
local function len(...t) return #t end
print(len(1, 2, 3))
local function idx(...t) return t[...] end
print(idx(2, "second"))
print(load("local function f(...t) t = 1 end"))
print(load("local function f(...t) function t() end end"))
print(load("local function f(...t) for t = 1, 2 do end end return 1") ~= nil)
