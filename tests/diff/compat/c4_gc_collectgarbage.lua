-- collectgarbage's options, results and errors, outside and inside a
-- finalizer.
print(collectgarbage("incremental"))
print(collectgarbage(), collectgarbage("collect"))
print(math.type(collectgarbage("count")), collectgarbage("count") > 0)
print(collectgarbage("isrunning"))
print(collectgarbage("stop"), collectgarbage("isrunning"), collectgarbage("restart"), collectgarbage("isrunning"))
print(select("#", collectgarbage("stop")), select("#", collectgarbage("restart")), select("#", collectgarbage()))
print(collectgarbage("incremental"), collectgarbage("generational"), collectgarbage("generational"), collectgarbage("incremental"))
print(type(collectgarbage("step")), type(collectgarbage("step", 0)), type(collectgarbage("step", 100)))
print(collectgarbage("generational"), collectgarbage("step"), collectgarbage("step", 5), collectgarbage("incremental"))

-- Errors.
print(pcall(collectgarbage, "foo"))
print(pcall(collectgarbage, "setpause", 100))
print(pcall(collectgarbage, 1))
print(pcall(collectgarbage, {}))
print(pcall(collectgarbage, "step", "x"))
print(pcall(collectgarbage, "param", "bogus"))
print(pcall(collectgarbage, "param"))
print(pcall(collectgarbage, "param", "pause", "x"))

-- Parameters read back what was set; a negative value only reads.
print(collectgarbage("param", "pause"), collectgarbage("param", "stepmul"))
local opause = collectgarbage("param", "pause", 100)
local ostepmul = collectgarbage("param", "stepmul", 100)
print(collectgarbage("param", "pause"), collectgarbage("param", "stepmul"))
print(collectgarbage("param", "pause", 7), collectgarbage("param", "pause", -1), collectgarbage("param", "pause"))
-- The generational defaults are our collector's own (DEVIATIONS.md), so
-- print only what setting and restoring them returns.
for _, p in ipairs{"minormul", "majorminor", "minormajor"} do
  local old = collectgarbage("param", p, 100)
  print(p, math.type(old), collectgarbage("param", p),
    collectgarbage("param", p, old), collectgarbage("param", p) == old)
end
local t = {0, 2, 10, 90, 500, 5000, 30000}
for i = 1, #t do
  collectgarbage("param", "pause", t[i])
  for j = 1, #t do
    collectgarbage("param", "stepmul", t[j])
    collectgarbage("step", t[j])
  end
end
collectgarbage("param", "pause", opause)
collectgarbage("param", "stepmul", ostepmul)
print(collectgarbage("param", "pause") == opause, collectgarbage("param", "stepmul") == ostepmul)

-- stepsize 0: every step finishes a cycle.
local step = collectgarbage("param", "stepsize", 0)
print(collectgarbage("param", "stepsize"), collectgarbage("step"), collectgarbage("step"))
collectgarbage("param", "stepsize", step)
print(collectgarbage("param", "stepsize") == step)

-- Steps work while the collector is stopped and don't restart it; bigger
-- steps finish a cycle in fewer calls.
local function dosteps(siz)
  collectgarbage()
  local a = {}
  for i = 1, 100 do a[i] = {{}}; local b = {} end
  local x = collectgarbage("count")
  local i = 0
  repeat i = i + 1 until collectgarbage("step", siz)
  return i, collectgarbage("count") < x
end
collectgarbage("stop")
local s10, less10 = dosteps(10)
local s2, less2 = dosteps(2)
print("steps", s10 < s2, less10, less2, collectgarbage("isrunning"))
collectgarbage("restart")

-- Inside a finalizer every option returns nil and changes nothing.
local function inside(...)
  local args = {...}
  local nargs = select("#", ...)
  local res, n
  setmetatable({}, {__gc = function ()
    local function grab(...) return {...}, select("#", ...) end
    res, n = grab(collectgarbage(table.unpack(args, 1, nargs)))
  end})
  collectgarbage()
  local parts = {}
  for i = 1, n do parts[i] = tostring(res[i]) end
  print("inside", args[1], n, table.concat(parts, " "))
end
for _, o in ipairs{"collect", "step", "count", "stop", "restart", "isrunning", "incremental", "generational"} do
  inside(o)
end
inside("param", "pause")
inside("param", "pause", 120)
print(collectgarbage("isrunning"), collectgarbage("param", "pause") == opause, collectgarbage("incremental"))

-- The collector isn't re-entered from a finalizer (gc.lua).
local res = true
setmetatable({}, {__gc = function () res = collectgarbage() end})
collectgarbage()
print("reentrant", res)

-- Generational mode: a step collects garbage and runs finalizers.
collectgarbage("generational")
local fin = false
setmetatable({}, {__gc = function () fin = true end})
local w = setmetatable({}, {__mode = "v"})
w[1] = {}
print("gen step", collectgarbage("step"), fin, w[1])
collectgarbage("incremental")
