-- Garbage that lua5.5 frees and nothing should keep alive: the old value of
-- a captured local changed by a closure or a multiple assignment, the cells
-- of captured locals after their scope or loop ends, and the locals of
-- frames a caught error unwound. Each case also checks the values still
-- read right.
local weak = setmetatable({}, {__mode = "k"})
local function gone(name)
  collectgarbage()
  print(name, next(weak) == nil)
  for k in pairs(weak) do weak[k] = nil end
end

-- A captured local an inner function assigns.
do
  local b = {}
  weak[b] = true
  local function clear() b = nil end
  local function get() return b end
  clear()
  gone("assigned by a closure")
  print(get())
end

do
  local n, s, t = 1, "s", {}
  weak[t] = true
  local function set(v) n, s, t = v, v, v end
  local function get() return n, s, t end
  set(false)
  gone("several, assigned by a closure")
  print(get())
end

local function param(p)
  weak[p] = true
  local function clear() p = nil end
  clear()
  gone("parameter assigned by a closure")
  return function () return p end
end
print(param({})())

-- A counter: its value stays right whoever assigns it.
do
  local count = 0
  local function inc() count = count + 1 end
  inc(); inc()
  count = count + 10
  inc()
  print("counter", count)
end

-- Multiple assignments to captured locals.
do
  local b, c = {}, 1
  weak[b] = true
  local function get() return b, c end
  b, c = nil, 2
  gone("multiple assignment")
  print(get())
end

do
  local b, c = {}, 1
  weak[b] = true
  local function get() return b, c end
  b, c = nil
  gone("adjusted multiple assignment")
  print(get())
end

do
  local b, c = 1, 2
  local function get() return b, c end
  b, c = c, b
  print("swap", get())
  b, b = 3, 4
  print("twice", get())
  b, c = (function () return 5, 6, 7 end)()
  print("call", get())
end

-- Cells of loop variables and of locals declared in loops.
do
  local f
  for i, v in ipairs({{}}) do
    weak[v] = true
    f = function () return v end
  end
  f = nil
  gone("generic for variable")
end

do
  local f
  for i = 1, 3 do
    local t = {}
    weak[t] = true
    f = function () return t, i end
  end
  f = nil
  gone("numeric for body")
end

do
  local f, i = nil, 0
  while i < 3 do
    i = i + 1
    local t = {}
    weak[t] = true
    f = function () return t end
    if i == 2 then break end
  end
  f = nil
  gone("while with break")
end

do
  local f, i = nil, 0
  repeat
    i = i + 1
    local t = {}
    weak[t] = true
    f = function () return t end
  until t and i == 3
  f = nil
  gone("repeat")
end

do
  local f
  for i = 1, 2 do
    for j = 1, 2 do
      local t = {}
      weak[t] = true
      f = function () return t end
    end
  end
  f = nil
  gone("nested loops")
end

do
  local f
  do
    local t = {}
    weak[t] = true
    f = function () return t end
  end
  f = nil
  gone("block")
end

-- Closures made in loops still see their own variables.
do
  local fs = {}
  for i = 1, 3 do
    local j = i * 10
    fs[i] = function () j = j + 1; return i, j end
  end
  print(fs[1](), fs[2](), fs[3](), fs[1]())
  local i = 0
  while i < 2 do
    i = i + 1
    local k = i
    fs[i] = function () return k end
  end
  print(fs[1](), fs[2]())
end

-- The locals of frames an error unwound to a pcall. The check reads only
-- a length, so it holds no reference itself.
local wv = setmetatable({}, {__mode = "v"})
local function deep(n)
  if n == 0 then
    local a, b, c, d, e, f, g, h = 1, 2, 3, 4, 5, 6, 7, 8
    local t = {}
    wv[1] = t
    local z = nil + 1
  end
  local r = deep(n - 1)
  return r
end
local function unwound()
  print(pcall(deep, 4))
  local freed = false
  for i = 1, 1000000 do
    local _ = {}
    if #wv == 0 then freed = true; break end
  end
  local x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15, x16,
        x17, x18, x19, x20, x21, x22, x23, x24, x25, x26, x27, x28, x29, x30 = 0
  return freed
end
print("unwound by an error", unwound())
