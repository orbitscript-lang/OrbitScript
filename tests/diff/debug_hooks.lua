-- debug.sethook and debug.gethook: call, return, line and count events.
-- Uses only the base library and table.concat. Count events are only
-- checked where they don't depend on how many instructions a statement
-- compiles to.

local out = {}
local function rec(...)
  local t = {}
  for i = 1, select("#", ...) do t[i] = tostring((select(i, ...))) end
  out[#out + 1] = table.concat(t, ":")
end
local function flush(label)
  print(label, table.concat(out, " "))
  out = {}
end

-- Lines, including loops and calls.
local function g(x)
  return x + 1
end
local function lines(e, l) rec(e, l) end
debug.sethook(lines, "l")
local s = 0
for i = 1, 3 do
  s = s + g(i)
end
while s > 10 do s = s - 4 end
repeat
  s = s - 1
until s < 10
if s > 0 then
  s = 0
else
  s = 1
end
debug.sethook()
flush("lines")

-- Calls, returns and tail calls, with names.
local function named(e)
  local i = debug.getinfo(2, "nS")
  rec(e, i.name or "?", i.what)
end
local function leaf(a) return a end
local function tail(a) return leaf(a) end
local function mid(a) local r = tail(a); return r end
debug.sethook(named, "cr")
mid(1)
print("in between")
local t = setmetatable({}, {__index = function (_, k) return k end})
local _ = t.x
pcall(error, "e")
debug.sethook()
flush("calls")

-- Transferred values.
local on = false
local function transfer(e)
  if not on then return end
  local ar = debug.getinfo(2, "r")
  local t = {}
  for i = ar.ftransfer, ar.ftransfer + ar.ntransfer - 1 do
    local _, v = debug.getlocal(2, i)
    t[#t + 1] = tostring(v)
  end
  rec(e, ar.ftransfer, ar.ntransfer, table.concat(t, ","))
end
local function foo(a, ...) return ... end
local function foo1(x) return foo(20, 10, x) end
-- sethook's own return isn't recorded: the reference's ftransfer for it
-- reflects its C implementation's stack.
debug.sethook(transfer, "cr")
on = true
foo1(0)
select(2, 10, 20, 30)
local function three() local a, b = 7, 8; return a, b, 9 end
three()
debug.sethook()
flush("transfer")

-- The hook's own level, name and traceback.
local function inspect(e, l)
  local h = debug.getinfo(1, "nSl")
  local f = debug.getinfo(2, "nSl")
  rec(e, l, h.namewhat, h.name, f.name, f.currentline)
  debug.sethook()
  print(debug.traceback("tb"))
end
local function victim() return 1 end
debug.sethook(inspect, "c")
victim()
flush("inspect")

-- gethook, masks and counts.
print(debug.gethook())
debug.sethook(rec, "rlc", 7)
local f, m, c = debug.gethook()
debug.sethook()
print(f == rec, m, c)
debug.sethook(rec, "", 0)
print(debug.gethook())
debug.sethook(rec, "x", -2)
print(debug.gethook())
debug.sethook(rec, "l", -2)
f, m, c = debug.gethook()
debug.sethook()
print(f == rec, m, c)
print(pcall(debug.sethook, 1, "l"))
print(pcall(debug.sethook, print, {}))
print(pcall(debug.sethook, print, "l", "x"))
out = {}

-- Count events in an empty loop: one per iteration plus a few.
local n = 0
debug.sethook(function () n = n + 1 end, "", 1)
for i = 1, 1000 do end
debug.sethook()
print(n > 1000 and n < 1012)
n = 0
debug.sethook(function () n = n + 1 end, "", 4)
for i = 1, 1000 do end
debug.sethook()
print(n > 248 and n < 256)

-- Errors in hooks, and a hook that turns itself off.
print(pcall(function ()
  debug.sethook(function () debug.sethook(); error("in hook") end, "l")
  local x = 1
  return x
end))
print(debug.gethook())
local calls = 0
debug.sethook(function ()
  calls = calls + 1
  debug.sethook()
end, "l")
local y = 1
y = y + 1
print(calls, debug.gethook())

-- Hooks don't fire inside a hook.
local depth = 0
debug.sethook(function (e)
  depth = depth + 1
  g(1)
end, "c")
g(2)
debug.sethook()
print(depth)
