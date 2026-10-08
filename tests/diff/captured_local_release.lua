-- A captured local's old value is garbage once the local is reassigned:
-- nothing else (such as the register its initializer was computed in)
-- keeps it alive.
local weak = setmetatable({}, {__mode = "k"})
local function gone(name)
  collectgarbage()
  print(name, next(weak) == nil)
  for k in pairs(weak) do weak[k] = nil end
end

do
  local b = {}
  weak[b] = true
  local function get() return b end
  b = nil
  gone("local")
  print(get())
end

local function param(p)
  weak[p] = true
  local function get() return p end
  p = nil
  gone("parameter")
  return get()
end
print(param({}))

-- Captured generic-for variables still read their own values.
do
  local fs = {}
  for i, v in ipairs({"a", "b"}) do fs[i] = function () return v end end
  print(fs[1](), fs[2]())
end

-- A captured <close> local is still closed, with its value.
do
  local log = {}
  do
    local c <close> = setmetatable({}, {__close = function (o) log[1] = o end})
    local function get() return c end
    assert(get() == c)
  end
  print("closed", log[1] ~= nil)
end

-- Numeric for and many captured locals still read their values.
local fs = {}
for i = 1, 3 do fs[i] = function () return i end end
print(fs[1](), fs[2](), fs[3]())
local x, y, z = 1, 2, 3
local function sum() return x + y + z end
print(sum())

-- Without debug information, debug.getlocal reads the register, which
-- follows assignments to a captured local.
local prog = [[
local a = 1
local get = function () return a end
a = 2
local n, v = debug.getlocal(1, 1)
return n, v, get()
]]
print(load(string.dump(load(prog), true))())
