-- Library functions that call back into script code (gsub, concat, unpack,
-- move, format, tostring, math.max, load's reader): depth limits, errors,
-- yields refused, and a coroutine closing itself from inside a callback.

-- coroutine.close() inside callbacks
local function run(body)
  local co = coroutine.create(body)
  print(coroutine.resume(co))
  print(coroutine.status(co))
end
run(function() string.gsub("a", "a", function() coroutine.close() end) print("no") end)
run(function()
  local x <close> = setmetatable({}, {__close=function(_, e) print("closing", e) end})
  print(pcall(string.gsub, "a", "a", function() coroutine.close() end)) print("no") end)
run(function()
  local x <close> = setmetatable({}, {__close=function(_, e) error("in close") end})
  print(pcall(string.gsub, "a", "a", function() coroutine.close() end)) print("no") end)
run(function()
  local t = setmetatable({}, {__index=function() coroutine.close() end})
  print(pcall(table.concat, t, ",", 1, 2)) print("no") end)
run(function()
  print(pcall(load, function() coroutine.close() end)) print("no") end)
run(function()
  local x <close> = setmetatable({}, {__close=function(_, e) print("outer", e) end})
  string.gsub("a", "a", function()
    local y <close> = setmetatable({}, {__close=function(_, e) print("inner", e) end})
    coroutine.close() end) print("no") end)

-- nesting until the C stack limit
local depth = 0
local function f() depth = depth + 1; return (string.gsub("a", "a", f)) end
print(pcall(f)); print(depth)
depth = 0
local t = setmetatable({}, {__index = function(t, k) depth = depth + 1; return table.concat(t, ",", 1, 1) end})
print(pcall(table.concat, t, ",", 1, 1)); print(depth)
depth = 0
local function g() depth = depth + 1; local ok, e = pcall(string.gsub, "a", "a", g); return e end
print(g()); print(depth)
local co = coroutine.wrap(function() depth = 0; print(pcall(f)); print(depth) end); co()

-- errors pass through, with their values
print(pcall(string.gsub, "abc", "%w", function(c) if c == "b" then error({c}) end end))
print(select(2, pcall(string.gsub, "abc", "%w", function(c) error("bad " .. c) end)))
local p = setmetatable({}, {__index = function(_, k) if k == 3 then error("at " .. k) end return k end})
print(pcall(table.concat, p, ",", 1, 4))
print(pcall(table.unpack, p, 1, 4))
print(pcall(math.max, setmetatable({}, {__lt = function() error("lt") end}), {}))
print(pcall(string.format, "%s", setmetatable({}, {__tostring = function() return 1 end})))
print(pcall(tostring, setmetatable({}, {__tostring = function() return {} end})))
-- (lua5.5 adds a traceback to these two messages; only their first lines
-- are compared)
print((select(2, load(function() error("reader") end)):match("[^\n]*")))
print((select(2, load(function() return {} end)):match("[^\n]*")))

-- yields are refused across them
local co = coroutine.wrap(function()
  print(pcall(string.gsub, "a", "a", function() coroutine.yield(1) end))
  local q = setmetatable({}, {__index = function() return coroutine.yield(2) end})
  print(pcall(table.concat, q, ",", 1, 2))
  print(pcall(math.min, setmetatable({}, {__lt = function() coroutine.yield(3) end}), {}))
  print(pcall(load, function() coroutine.yield(4) end))
  return "done"
end)
print(co())

-- results and extra arguments
print(string.gsub("hello world", "(o)(.)", function(a, b) return b .. a end))
print(string.gsub("abc", "%w", function() end))
print(string.gsub("abc", "%w", function() return false end))
print(table.concat(setmetatable({}, {__index = function(_, k) return k * 2 end, __len = function() return 4 end}), "-"))
local m = setmetatable({}, {__lt = function(a, b, extra) return extra == nil end})
print(math.max(m, setmetatable({}, getmetatable(m))) == m)
local moved = table.move(setmetatable({}, {__index = function(_, k) return k end}), 1, 3, 2, {})
print(moved[2], moved[3], moved[4])
