-- Metamethods through every VM path, without the standard libraries.
local log = {}

local mt = {}
mt.__index = function (t, k) return "idx:" .. tostring(k) end
mt.__newindex = function (t, k, v) rawset(t, k, v and v * 2) end
mt.__call = function (self, a, b) return "called", a, b end
mt.__add = function (a, b) return "add" end
mt.__sub = function (a, b) return "sub" end
mt.__mul = function (a, b) return "mul" end
mt.__div = function (a, b) return "div" end
mt.__mod = function (a, b) return "mod" end
mt.__pow = function (a, b) return "pow" end
mt.__unm = function (a, b) return "unm", rawequal(a, b) end
mt.__idiv = function (a, b) return "idiv" end
mt.__band = function (a, b) return "band" end
mt.__bor = function (a, b) return "bor" end
mt.__bxor = function (a, b) return "bxor" end
mt.__shl = function (a, b) return "shl" end
mt.__shr = function (a, b) return "shr" end
mt.__bnot = function (a, b) return "bnot" end
mt.__concat = function (a, b) return "cat(" .. tostring(type(a)) .. "," .. tostring(type(b)) .. ")" end
mt.__len = function (a) return 42 end
mt.__eq = function (a, b) return true end
mt.__lt = function (a, b) return true end
mt.__le = function (a, b) return false end
mt.__tostring = function () return "OBJ" end
local a = setmetatable({}, mt)
local b = setmetatable({}, mt)
print(a.foo, a[1], a[2.0], a[true])
a.x = 21; a[3] = 4
print(rawget(a, "x"), rawget(a, 3))
print(a(1, 2))
print(a + 1, 1 + a, a - 1, a * 1, a / 1, a % 1, a ^ 1, a // 1)
print(a & 1, 1 | a, a ~ 1, a << 1, 1 >> a, ~a, -a)
print(a + 1.5, a + 100, a - 100, 5 & a, a .. "s", "s" .. a, 1 .. a, a .. b)
print("x" .. "y" .. a .. "z", #a, a == b, a ~= b, a == a, a < b, a <= b, a > b, a >= b)
print(a < 1, 1 < a, a <= 1, 1 >= a, a > 1)
print(tostring(a), a == {}, rawequal(a, b), rawlen(a))
-- __index / __newindex chains through tables
local base = {greet = "hi"}
local mid = setmetatable({}, {__index = base})
local top = setmetatable({}, {__index = mid})
print(top.greet, top.nothing)
local sink = {}
local w = setmetatable({}, {__newindex = sink})
w.k = 1
print(rawget(w, "k"), sink.k)
-- __eq only between tables/userdata, not across types
local e1 = setmetatable({}, {__eq = function () return true end})
print(e1 == 1, e1 == "x", e1 == setmetatable({}, {}))
-- __lt result is converted to boolean
local c = setmetatable({}, {__lt = function () return 1 end, __le = function () return nil end})
print(c < c, c <= c, c > c, c >= c)
-- __call chains and non-callable
local callee = setmetatable({}, {__call = setmetatable({}, {__call = function (...) return select("#", ...) end})})
print(pcall(callee, "x"))
print(pcall(setmetatable({}, {__call = 1})))
-- __metatable protection
local p = setmetatable({}, {__metatable = "locked"})
print(getmetatable(p), pcall(setmetatable, p, {}))
-- __close and __gc presence checks
print(pcall(function () local x <close> = {} end))
print(pcall(function () local x <close> = 42 end))
print(pcall(function () local x <close> = false end))
-- __tostring must return a string; __name is used
print(pcall(tostring, setmetatable({}, {__tostring = function () return {} end})))
local named = setmetatable({}, {__name = "MyType"})
print(type(tostring(named)))
print(pcall(function () return named + 1 end))
print(pcall(function () return #setmetatable({}, {__name = 1}) end))
-- __len result can be anything
print(#setmetatable({}, {__len = function () return "long" end}))
-- __concat with numbers on the left, and multiple operands
local cc = setmetatable({}, {__concat = function (x, y)
  local tx = type(x) == "table" and "T" or tostring(x)
  local ty = type(y) == "table" and "T" or tostring(y)
  return tx .. ty
end})
print(1 .. 2 .. cc .. 3 .. 4, cc .. cc)
-- __index function receives the original table, rawget bypasses
local orig = setmetatable({}, {__index = function (t, k) return t end})
print(orig.q == orig, rawget(orig, "q"))
-- deep __index chain error
local loop = {}
setmetatable(loop, {__index = loop})
print(pcall(function () return loop.x end))
