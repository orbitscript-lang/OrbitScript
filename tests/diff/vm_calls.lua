-- Calls, varargs, multiple results, tail calls and recursion limits.
local function pack(...) return {n = select("#", ...), ...} end
local function count(...) return select("#", ...) end
local function three() return 1, 2, 3 end
print(count(), count(nil), count(nil, nil), count(three()), count(three(), 10), count((three())))
print(select(2, "a", "b", "c"), select(-1, "a", "b", "c"), select("#"))
print(pcall(select, 0, 1))
print(pcall(select, -5, 1))
local t = {three(), three()}
print(#t, t[4])
local t2 = {three(), (three())}
print(#t2)
local function va(a, ...)
  local x, y = ...
  return a, x, y, select("#", ...)
end
print(va(1), va(1, 2), va(1, 2, 3, 4))
local function tail(n) if n == 0 then return "done" end return tail(n - 1) end
print(tail(100000))
local function tailva(n, ...) if n == 0 then return ... end return tailva(n - 1, ...) end
print(tailva(1000, "x", "y"))
local depth = 0
local function rec() depth = depth + 1; return 1 + rec() end
local ok, msg = pcall(rec)
print(ok, msg, depth > 1000)
local function inf() return inf() end
-- method calls and self
local obj = {v = 5}
function obj:get(d) return self.v + (d or 0) end
function obj.static(x) return x end
print(obj:get(), obj:get(2), obj.static(7), obj.get(obj, 1))
-- varargs in table constructors and returns
local function mid(...) return ..., "end" end
print(mid(1, 2, 3))
print(pack(mid()).n, pack(mid(nil, nil)).n)
-- named varargs (Lua 5.5)
local function nv(...t) return t.n, t[1], t[2] end
print(nv(), nv("a", nil))
local function nv2(a, ...rest) rest[1] = "changed"; return a, ... end
print(nv2(1, 2, 3))
-- long argument lists
local function sum(...)
  local s = 0
  for i = 1, select("#", ...) do s = s + (select(i, ...)) end
  return s
end
local big = {}
for i = 1, 200 do big[i] = i end
print(sum(table_unpack_guard or 1, 2, 3))
-- closures and upvalues shared between siblings
local function counter()
  local c = 0
  return function () c = c + 1; return c end, function () return c end
end
local inc, get = counter()
inc(); inc()
print(get())
local fs = {}
for i = 1, 3 do fs[i] = function () return i end end
print(fs[1](), fs[2](), fs[3]())
local ws = {}
local j = 1
while j <= 3 do local k = j; ws[j] = function () k = k + 10; return k end; j = j + 1 end
print(ws[1](), ws[1](), ws[2]())
-- calling non-functions
print(pcall(function () local x = 1; x() end))
print(pcall(function () undefinedglobal() end))
print(pcall(function () local t = {}; t.field.sub() end))
print(pcall(function () local t = {}; t:method() end))
print(pcall(function () ("x")() end))
