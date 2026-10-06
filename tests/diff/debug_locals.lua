-- debug.getlocal, setlocal, getupvalue, setupvalue, upvalueid, upvaluejoin.

local function dump(level)
  local i = 1
  while true do
    local n, v = debug.getlocal(level + 1, i)
    if not n then break end
    if type(v) == "function" or type(v) == "table" then v = type(v) end
    print("", i, n, v)
    i = i + 1
  end
end

print("main"); dump(1)
for i = 1, 2 do local x = i * 10; print("numeric for"); dump(1) end
for k, v in pairs({a = 1}) do print("generic for"); dump(1) end
local function f(a, b, ...)
  local c = 3
  print("f"); dump(1)
  print(debug.getlocal(1, -1)); print(debug.getlocal(1, -2)); print(debug.getlocal(1, -3))
  print(debug.setlocal(1, -1, "changed"), (...))
end
f(1, 2, 9, 8)
f(1)
local function g(a, b) return (a + 1) + (function () dump(2); return 0 end)() end
g(10, 20)
local function h()
  local q <const> = 5
  local r <const> = {}
  local captured = 7
  local function use() return captured end
  print("h"); dump(1)
  print(debug.setlocal(1, 2, 70), captured, use())
end
h()

-- Parameters of a function value.
print(debug.getlocal(f, 1), debug.getlocal(f, 2), debug.getlocal(f, 3), debug.getlocal(print, 1))

-- Native windows and bad levels.
print(debug.getlocal(0, 1)); print(debug.getlocal(0, 2)); print(debug.getlocal(0, 3))
print(pcall(debug.getlocal, 20, 1))
print(pcall(debug.setlocal, -1, 1, 10))
print(pcall(debug.getlocal, 1))

-- Locals of a caller, written through setlocal.
local function setter() print(debug.setlocal(2, 1, "set"), debug.setlocal(2, 9, 1)) end
local function owner(x) setter(); return x end
print(owner("orig"))

-- Upvalues.
local a, b, c = 1, 2, 3
local function foo1(x) b = x; return c end
local function foo2(x) a = x; return c + b end
local function ups(fn)
  local i = 1
  while true do
    local n, v = debug.getupvalue(fn, i)
    if not n then break end
    print("", i, n, v)
    i = i + 1
  end
end
print("foo1"); ups(foo1)
print("foo2"); ups(foo2)
print(debug.getupvalue(foo1, 0), debug.getupvalue(foo1, 3), select("#", debug.getupvalue(foo1, 3)))
print(debug.setupvalue(foo1, 1, "xuxu"), b, foo2 ~= nil)
print(debug.setupvalue(foo1, 3, 1))
print(debug.getupvalue(print, 1))
print(pcall(debug.getupvalue, {}, 1))

print(type(debug.upvalueid(foo1, 1)))
print(debug.upvalueid(foo1, 1) == debug.upvalueid(foo2, 2))
print(debug.upvalueid(foo1, 2) == debug.upvalueid(foo2, 3))
print(debug.upvalueid(foo1, 1) == debug.upvalueid(foo1, 2))
print(debug.upvalueid(foo1, 5), pcall(debug.upvalueid, print, 1))
local ids = {[debug.upvalueid(foo1, 1)] = "b's cell"}
print(ids[debug.upvalueid(foo2, 2)])

debug.upvaluejoin(foo1, 2, foo2, 1)   -- foo1's 'c' is now foo2's 'a'
print(foo1(5), a, c)
print(pcall(debug.upvaluejoin, foo1, 3, foo2, 1))
print(pcall(debug.upvaluejoin, foo1, 1, foo2, 4))
print(pcall(debug.upvaluejoin, print, 1, foo2, 1))
print(pcall(debug.upvaluejoin, foo1, 1, print, 1))

-- A self-referencing local function keeps working after its own upvalue
-- changes.
local function rec(n) if n == 0 then return "base" end return rec(n - 1) end
local i = 1
while debug.getupvalue(rec, i) ~= "rec" do i = i + 1 end
print(rec(3), debug.setupvalue(rec, i, function () return "replaced" end), rec(3))
local function rec2(n) if n == 0 then return "base2" end return rec2(n - 1) end
local other = function () return "joined" end
local function holder() return other end
debug.upvaluejoin(rec2, 1, holder, 1)
print(rec2(2))

-- Metatables and user values.
print(debug.getmetatable(1), debug.getmetatable({}))
local mt = {__index = function (_, k) return k .. "!" end}
print(debug.setmetatable(10, mt) == 10, (5).x, debug.getmetatable(3) == mt)
debug.setmetatable(10, nil)
print(pcall(function () return (5).x end))
local prot = setmetatable({}, {__metatable = "locked"})
print(getmetatable(prot), type(debug.getmetatable(prot)))
print(pcall(debug.setmetatable, {}, 1))
print(debug.getuservalue({}), debug.getuservalue(1, 2))
print(pcall(debug.setuservalue, {}, 1))
print(pcall(debug.setuservalue, debug.upvalueid(foo1, 1), {}))
print(type(debug.getregistry()), debug.getregistry()[2] == _G)
