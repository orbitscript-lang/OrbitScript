-- Error values, levels, message handlers and to-be-closed variables.
print(pcall(error))
print(pcall(error, nil))
print(pcall(error, "msg", 0))
print(pcall(error, "msg", 1))
local function lvl2() error("deep", 2) end
local function caller() lvl2() end
print(pcall(caller))
print(pcall(error, {code = 1}))
print(select("#", pcall(error)))
print(pcall(assert, false))
print(pcall(assert, nil, "custom"))
print(pcall(assert, false, {1}))
print(assert(1, 2, 3))
print(pcall(assert))
print(xpcall(function () error("E") end, function (m) return "handled: " .. m end))
print(xpcall(function (...) return ... end, print, 1, 2, 3))
print(xpcall(function () error({}) end, function (m) return type(m) end))
print(pcall(xpcall))
print(pcall(pcall))
-- errors inside the message handler
print(xpcall(function () error("first") end, function (m) error("second") end))
-- nested pcall keeps the stack consistent
local r = {pcall(function ()
  local a, b = pcall(error, "inner")
  return a, b, "after"
end)}
print(r[1], r[2], r[3], r[4])
-- runtime errors carry positions
print(pcall(function () local t = nil; return t.x end))
print(pcall(function () return nil + 1 end))
local up = {}
print(pcall(function () return up.a.b end))
print(pcall(function () return ({}).x.y end))
print(pcall(rawset, {}, nil, 1))
print(pcall(rawset, {}, 0/0, 1))
print(pcall(function () local t = {}; t[nil] = 1 end))
print(pcall(function () local t = {}; t[0/0] = 1 end))
print(pcall(setmetatable, 1, {}))
print(pcall(setmetatable, {}, 1))
print(pcall(next, {}, "nokey"))
print(pcall(rawlen, 5))
print(pcall(getmetatable))
-- to-be-closed variables: order, errors and error replacement
do
  local function closer(name) return setmetatable({}, {__close = function (_, e) print("close", name, e) end}) end
  do
    local a <close> = closer("a")
    local b <close> = closer("b")
    local c <close> = nil
    print("body")
  end
  print(pcall(function ()
    local a <close> = closer("a2")
    error("boom", 0)
  end))
  print(pcall(function ()
    local a <close> = setmetatable({}, {__close = function () error("in close", 0) end})
    local b <close> = closer("b3")
    error("orig", 0)
  end))
  local function ret()
    local x <close> = closer("ret")
    return "value"
  end
  print(ret())
  for i = 1, 2 do
    local y <close> = closer("loop" .. i)
    if i == 2 then break end
  end
  local function gen(n)
    local i = 0
    return function () i = i + 1; if i <= n then return i end end, nil, nil, closer("iter")
  end
  for v in gen(2) do print("iter", v) end
  for v in gen(5) do if v == 2 then break end end
end
local const <const> = 10
print(const)
-- error objects that are not strings pass through untouched
local eobj = setmetatable({}, {__tostring = function () return "EOBJ" end})
local ok, e = pcall(error, eobj)
print(ok, e == eobj, tostring(e))
-- error with level pointing into a native function
print(pcall(function () error("lvl", 3) end))
