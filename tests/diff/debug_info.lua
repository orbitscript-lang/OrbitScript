-- debug.getinfo, function names and debug.traceback.
-- Uses only the base library and table.concat, so it runs without the
-- string and table libraries.

local function sort(t)
  for i = 2, #t do
    local v, j = t[i], i - 1
    while j > 0 and t[j] > v do t[j + 1] = t[j]; j = j - 1 end
    t[j + 1] = v
  end
end

local function show(t)
  local keys = {}
  for k in pairs(t) do keys[#keys + 1] = k end
  sort(keys)
  local out = {}
  for _, k in ipairs(keys) do
    local v = t[k]
    if type(v) == "function" or type(v) == "table" then v = type(v) end
    out[#out + 1] = k .. "=" .. tostring(v)
  end
  print(table.concat(out, " "))
end

-- Functions given directly.
show(debug.getinfo(print))
show(debug.getinfo(show))
show(debug.getinfo(function (a, b, ...) end, "Su"))
show(debug.getinfo(1, "S"))
print(debug.getinfo(1000), debug.getinfo(-1))

-- Levels and names.
local function lf() return debug.getinfo(1, "nSl"), debug.getinfo(2, "nl") end
local a, b = lf()
show(a); show(b)
function gf() return debug.getinfo(1, "n") end
show(gf())
local t = {}
function t.field() return debug.getinfo(1, "n") end
function t:meth() return debug.getinfo(1, "n") end
show(t.field()); show(t:meth())
local up = function () return debug.getinfo(1, "n") end
local function viaup() return (up()) end
show(viaup())
for _ in function () show(debug.getinfo(1, "n")); return nil end do end
setmetatable(t, {__index = function () show(debug.getinfo(1, "n")); return 1 end,
                 __add = function () show(debug.getinfo(1, "n")); return 2 end})
local _ = t.zzz
_ = t + 1
show(debug.getinfo(1, "n"))

-- Tail calls.
local function tail() return debug.getinfo(1, "nt") end
local function caller() return tail() end
show(caller())
show(debug.getinfo(print, "t"))

-- Errors in options and arguments.
print(pcall(debug.getinfo, 1, "X"))
print(pcall(debug.getinfo, 1, ">"))
print(pcall(debug.getinfo, {}))
print(pcall(debug.getinfo))

-- Active lines.
local function lines(f)
  local t = debug.getinfo(f, "L").activelines
  local l = {}
  for k in pairs(t) do l[#l + 1] = k end
  sort(l)
  return table.concat(l, ",")
end
print(lines(function (a)
  local x = 1

  return x
end))
print(lines(function (...)
  local a = 20
  -- empty
  local b = 30
end))
print(lines(function (a, b) end))
print(debug.getinfo(print, "L").activelines)

-- Tracebacks.
local function tb(...) print(debug.traceback(...)) end
tb()
tb("msg")
tb("msg", 2)
tb("lvl0", 0)
print(debug.traceback(12))
print(debug.traceback({}) == nil, type(debug.traceback({})))
print(debug.traceback(nil))
print(pcall(debug.traceback))
local function deep(n, lv) if n == 0 then return debug.traceback("m", lv) else return (deep(n - 1, lv)) end end
for _, d in ipairs{19, 20, 21} do print(d, deep(d, 1)) end
print(deep(25, 1))
