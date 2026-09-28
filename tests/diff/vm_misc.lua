-- Control flow, tables, globals and chunk loading.
-- goto and labels
do
  local i = 1
  ::top::
  if i <= 3 then i = i + 1; goto top end
  print("goto loop", i)
  for j = 1, 3 do
    for k = 1, 3 do
      if k == 2 then goto continue end
      io_write_guard = nil
      ::continue::
    end
  end
  local fs = {}
  do
    local n = 0
    ::again::
    local m = n
    fs[#fs + 1] = function () return m end
    n = n + 1
    if n < 3 then goto again end
  end
  print(fs[1](), fs[2](), fs[3]())
end
-- repeat-until sees body locals
local n = 0
repeat local stop = n >= 2; n = n + 1 until stop
print("repeat", n)
-- while with break and nested closures capturing fresh locals
local caps = {}
local w = 0
while true do
  w = w + 1
  local v = w * 10
  caps[w] = function () return v end
  if w == 3 then break end
end
print(caps[1](), caps[2](), caps[3]())
-- numeric keys normalize
local t = {}
t[1.0] = "one"; t[2] = "two"; t[2^53] = "big"
print(t[1], t[2.0], t[2^53 | 0], #t)
t[1.5] = "frac"
print(t[1.5], t[3 / 2])
-- constructors: positional, keyed, trailing varargs, big lists
local function many() return 1, 2, 3 end
local c = {many(), many(), x = 1, [10] = 10, many()}
print(#c, c.x, c[10], c[5])
local src = "return {"
for i = 1, 600 do src = src .. i .. "," end
src = src .. "}"
local bigt = load(src)()
print(#bigt, bigt[1], bigt[300], bigt[600])
local src2 = "local t = {} "
for i = 1, 300 do src2 = src2 .. "t[" .. i .. "] = " .. i * 2 .. " " end
src2 = src2 .. "return t"
print(#load(src2)(), load(src2)()[300])
-- many locals and deep expressions
local e = "1"
for i = 1, 150 do e = "(" .. e .. " + 1)" end
print(load("return " .. e)())
-- multiple assignment evaluates all expressions before assigning
local a, b = 1, 2
a, b = b, a
print(a, b)
local arr = {10, 20, 30}
local idx = 1
idx, arr[idx] = 2, "x"
print(idx, arr[1], arr[2])
-- globals and _ENV
x_global = 5
print(x_global, _G.x_global, _ENV == _G)
do
  local _ENV = {print = print}
  y_global = 7
  print(y_global, x_global)
end
print(y_global)
local function withenv()
  local _ENV = setmetatable({}, {__index = _G})
  z = 1
  return z, _G.z
end
print(withenv())
-- load with custom environments, chunk names and modes
local env = {val = 42}
local f = load("return val", "=chunk", "t", env)
print(f())
print(load("return ...", "=varargs")(1, 2, 3))
print(load("syntax error here"))
print(load("x = ", "=named"))
print(load("return 1", "=m", "b"))
print(load(function () return nil end))
local parts = {"return ", "1 ", "+ 41"}
local pi = 0
print(load(function () pi = pi + 1; return parts[pi] end)())
print(pcall(load, "return 1", "c", "x"))
-- strings: long literals, escapes, comparison
local long = [[
line1
line2]]
print(long, #long)
print("\65\066\x43\u{48}\z
       I", "a\0b" < "a\0c", "a" < "ab", "" < "a", "Z" < "a")
print(#"\u{7FFFFFFF}", "\u{10FFFF}" == "\xF4\x8F\xBF\xBF")
-- integer/float keys in next() traversal of an array
local seq = {"a", "b", "c"}
local k, v = next(seq)
local order = {}
while k do order[#order + 1] = k .. "=" .. v; k, v = next(seq, k) end
print(order[1], order[2], order[3])
-- pairs with __pairs
local pp = setmetatable({}, {__pairs = function (t) return function (_, k) if not k then return 1, "only" end end, t, nil end})
for k2, v2 in pairs(pp) do print("pairs", k2, v2) end
-- ipairs stops at the first nil and respects __index
local ip = setmetatable({1, 2}, {__index = function (t, i) if i <= 4 then return i * 100 end end})
for i2, v3 in ipairs(ip) do print("ipairs", i2, v3) end
-- select('#') with many values and tostring of special values
print(tostring(nil), tostring(true), tostring(-0.0), tostring(1e100), tostring(-1/0), tostring(2^63))
print(math_guard, _VERSION)
