-- xpcall message handlers and warn (C3).

-- Only the base library: the string and table libraries live elsewhere.
local function show(...)
  local n, s = select("#", ...), ""
  for i = 1, n do
    s = s .. (i > 1 and " " or "") .. tostring((select(i, ...)))
  end
  print(n, s)
end

-- Success passes every result through.
show(xpcall(function(...) return ... end, print, 1, 2, 3))
show(xpcall(function() end, print))

-- The handler's first result is the error.
show(xpcall(error, function(m) return "H:" .. m end, "boom"))
show(xpcall(error, function(m) return 1, 2, 3 end, "e"))
show(xpcall(error, function() end, "e"))
show(xpcall(error, function() return nil end))
local t = {}
show(select(2, xpcall(error, function(m) return m end, t)) == t)
show(xpcall(function() error { msg = "x" } end, function(r) return r.msg .. "y" end))

-- The handler sees the error before anything is popped.
local depth = 0
local function deep(n)
  depth = n
  if n == 0 then error("bottom") end
  return deep(n - 1) + 1
end
local seen
show(xpcall(deep, function(m) seen = depth; return m end, 10))
print("depth seen by handler", seen)

-- Errors in the handler call it again, up to a limit.
show(xpcall(error, error))
show(xpcall(error, function(m) error("x") end))
local function err(n)
  if type(n) ~= "number" then return n
  elseif n == 0 then return "END"
  else error(n - 1)
  end
end
show(xpcall(error, err, 170))
show(xpcall(error, err, 300))

-- Nested xpcalls whose handlers recurse.
local function f(x)
  if x == 0 then error("a\n")
  else
    local aux = function() return f(x - 1) end
    return xpcall(aux, aux)
  end
end
show(f(3))

-- Stack overflow: the handler gets some room, and an overflow inside it
-- is an error in error handling.
local function loop(x, y, z) return 1 + loop(x, y, z) end
show(xpcall(loop, function(m)
  local ok, msg = pcall(loop)
  print("inside handler", ok, msg)
  return 15
end))
show(xpcall(loop, loop))
show(xpcall(loop, function(m) return "H:" .. m end))

-- Argument checks.
show(pcall(xpcall))
show(pcall(xpcall, print))
show(pcall(xpcall, print, 42))
show(xpcall(nil, function(m) return m end))

-- pcall inside the handler catches its own errors.
show(xpcall(error, function(m)
  return select(2, pcall(error, "inner")) .. "/" .. m
end, "outer"))

-- Yields cross xpcall, in the body and after a caught error.
local co = coroutine.wrap(function()
  return xpcall(function(a)
    local b = coroutine.yield(a)
    error({ a + b })
  end, function(x) return x[1] * 2 end, 10)
end)
show(co())
show(co(100))

local g = function(s, i) return coroutine.yield(i) end
local f1 = coroutine.wrap(function()
  return xpcall(pcall, function(...) return ... end, function()
    local s = 0
    for i in g, nil, 1 do pcall(function() s = s + i end) end
    error({ s })
  end)
end)
f1()
for i = 1, 10 do assert(f1(i) == i) end
local r1, r2, v = f1(nil)
show(r1, r2, v[1])

-- warn: off at start, switched by control messages.
warn("hidden")
warn("@on")
warn("a", "b", 3)
warn("@unknown")
warn("@x", "y")
warn("")
warn("@off")
warn("hidden again")
show(pcall(warn))
show(pcall(warn, "a", {}))
show(pcall(warn, "a", nil))
print("done")
