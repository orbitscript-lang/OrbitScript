-- The debug library's value functions: metatables, user values, the
-- registry and upvalues. Arguments are checked in the reference's order,
-- and integer indexes convert as a C int does.

local function show(...)
  print(select('#', ...), ...)
end

local function try(f, ...)
  show(pcall(f, ...))
end

local u = 1
local function f() return u end
local function g() return u end
local iter = string.gmatch("a", "a")

-- getmetatable, setmetatable
try(debug.getmetatable)
try(debug.getmetatable, nil)
print(debug.getmetatable("x") == getmetatable("x"))
try(debug.setmetatable)
try(debug.setmetatable, 1, 2)
do
  local t = {}
  local mt = {}
  print(debug.setmetatable(t, mt) == t, getmetatable(t) == mt)
  print(debug.setmetatable(t, nil) == t, getmetatable(t))
  print(debug.setmetatable(5, {__index = {five = true}}), (5).five)
  print(debug.setmetatable(5, nil), getmetatable(5))
end

-- getregistry
print(type(debug.getregistry()), debug.getregistry() == debug.getregistry())

-- getuservalue, setuservalue (io files have user value 1 here, none in
-- the reference, so only other indexes are compared)
try(debug.getuservalue)
try(debug.getuservalue, 1)
try(debug.getuservalue, 1, "x")
try(debug.getuservalue, io.stdout, 2)
try(debug.getuservalue, io.stdout, 1.5)
try(debug.setuservalue)
try(debug.setuservalue, 1)
try(debug.setuservalue, io.stdout)
try(debug.setuservalue, io.stdout, 1, 2)
try(debug.setuservalue, io.stdout, 1, "x")
try(debug.setuservalue, io.stdout, 1, 1.5)

-- getupvalue, setupvalue
try(debug.getupvalue)
try(debug.getupvalue, f)
try(debug.getupvalue, "x")
try(debug.getupvalue, "x", 1)
try(debug.getupvalue, f, 1.5)
try(debug.getupvalue, f, "1")
try(debug.getupvalue, f, 1)
try(debug.getupvalue, f, 2)
try(debug.getupvalue, f, 0)
try(debug.getupvalue, f, 1 << 40)
try(debug.getupvalue, f, -(1 << 40) + 1)
try(debug.getupvalue, print, 1)
try(debug.getupvalue, iter, 1)
try(debug.setupvalue)
try(debug.setupvalue, f, 1)
try(debug.setupvalue, "x", 1, 2)
try(debug.setupvalue, "x", "y", 2)
try(debug.setupvalue, f, nil, 2)
try(debug.setupvalue, f, 2, 1)
try(debug.setupvalue, print, 1, 2)
try(debug.setupvalue, f, 1, 5)
print(f())
try(debug.setupvalue, f, (1 << 32) + 1, 6)
print(f())

-- upvalueid
try(debug.upvalueid)
try(debug.upvalueid, f)
try(debug.upvalueid, "x", 1)
try(debug.upvalueid, f, 1.5)
try(debug.upvalueid, f, 2)
try(debug.upvalueid, f, 0)
try(debug.upvalueid, print, 1)
print(type(debug.upvalueid(f, 1)), type(debug.upvalueid(iter, 1)))
print(debug.upvalueid(f, 1) == debug.upvalueid(g, 1))
print(debug.upvalueid(f, (1 << 32) + 1) == debug.upvalueid(f, 1))

-- upvaluejoin
try(debug.upvaluejoin)
try(debug.upvaluejoin, "x", 1)
try(debug.upvaluejoin, "x", "y")
try(debug.upvaluejoin, f, 99, "x", 1)
try(debug.upvaluejoin, f, 1, "x", 1)
try(debug.upvaluejoin, f, 1, g)
try(debug.upvaluejoin, f, 1, "x")
try(debug.upvaluejoin, f, 1, g, 99)
try(debug.upvaluejoin, f, "x", g, 1)
try(debug.upvaluejoin, iter, 1, g, 1)
try(debug.upvaluejoin, iter, 1, g, 99)
try(debug.upvaluejoin, iter, 1, "x", 1)
try(debug.upvaluejoin, f, 1, iter, 1)
try(debug.upvaluejoin, f, 1, iter, 99)
do
  local a, b = 10, 20
  local function fa() return a end
  local function fb() return b end
  try(debug.upvaluejoin, fa, (1 << 32) + 1, fb, 1)
  print(fa(), fb(), debug.upvalueid(fa, 1) == debug.upvalueid(fb, 1))
end
