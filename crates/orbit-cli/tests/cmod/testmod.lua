-- Run by tests/c_modules.rs with testmod.so on package.cpath. Prints "ok"
-- at the end; the reference lua5.5 passes it too.
local t = require "testmod"

local a, b, c, n = t.reverse(1, "two", 3)
assert(a == 3 and b == "two" and c == 1 and n == 3)
assert(select("#", t.reverse()) == 1)

local ty, int, isstr, len = t.describe("12")
assert(ty == "string" and int == 12 and isstr and len == 2)
ty, int, isstr, len = t.describe(setmetatable({}, {__len = function() return 7 end}))
assert(ty == "table" and int == nil and not isstr and len == 7)

local z, zlen = t.zeros("a\0b")
assert(z == "a0b\0end" and zlen == 3)

assert(t.format("k", 5, 1.5):match("^k=5 %(1%.5%) x %% ."), t.format("k", 5, 1.5))

local seen = {}
local proxy = setmetatable({}, {
  __index = function(_, k) return "idx:" .. k end,
  __newindex = function(_, k, v) seen[k] = v end,
})
local got, made, count = t.tables(proxy)
assert(got == "idx:key" and made[2] == 20 and made.name == "named" and count == 4)
assert(seen.copied == made)

local r1, r2 = t.call(function(x, y) return x + y, x * y end, 3, 4)
assert(r1 == 7 and r2 == 12)
-- Errors from the called function go through the C function.
local ok, err = pcall(t.call, function() error("inner") end)
assert(not ok and err:find("inner"))

local status, msg = t.pcall(function() error("boom", 0) end)
assert(status == 2 and msg == "handled: boom")
status, msg = t.pcall(function() return "fine" end)
assert(status == 0 and msg == "fine")

ok, err = pcall(t.error, "table")
assert(not ok and type(err) == "table" and err.what == "payload")
ok, err = pcall(t.error, "aux")
assert(not ok and err == "aux error 42", err)
ok, err = pcall(t.error, "other")
assert(not ok and err == "bad argument #1 to 'testmod.error' (bad how)", err)
ok, err = pcall(t.zeros, {})
assert(not ok and err:find("string expected, got table"), err)

local step = t.counter(5)
assert(step() == 6 and step() == 7)
assert(debug.getupvalue(step, 1) == "")

do
  local p = t.point(1.5, 2)
  assert(p:sum() == 3.5)
  local v, tag = p:uv(1)
  assert(v == "first" and tag == 4)
  v, tag = p:uv(2)
  assert(v == "second")
  v, tag = p:uv(3)
  assert(v == nil and tag == -1)
  ok, err = pcall(p.sum, {})
  assert(not ok and err:find("testmod.Point expected"), err)
  assert(getmetatable(p) == debug.getregistry()["testmod.Point"])
end
local before = t.gccount()
for _ = 1, 10 do t.point(0, 0) end
collectgarbage()
collectgarbage()
assert(t.gccount() >= before + 10)

local val, isref = t.ref({1})
assert(val[1] == 1 and isref)

local st, res = t.load("return 6 * 7")
assert(st == 0 and res == 42)
st, res = t.load("return +")
assert(st == 3 and res:find("testchunk"), res)

local vec = setmetatable({}, {
  __add = function() return "added" end,
  __lt = function() return true end,
})
local sum, lt, req = t.arith(vec, 1)
assert(sum == "added" and lt == true and req == false)
sum, lt, req = t.arith(2, 2.0)
assert(sum == 4 and lt == false and req == true)

-- Functions from C modules are C functions to the debug library.
assert(debug.getinfo(t.reverse, "S").what == "C")
print("ok")
