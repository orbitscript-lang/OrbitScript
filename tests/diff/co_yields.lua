-- Coroutines: yields across pcall, metamethods and iterators, wrap
-- errors, status, close, and the nesting limit.
local co = coroutine.create(function (a, b)
  print("start", a, b)
  local c, d = coroutine.yield(a + b)
  print("got", c, d)
  return coroutine.yield(c * d)
end)
print(coroutine.status(co), coroutine.resume(co, 1, 2))
print(coroutine.status(co), coroutine.resume(co, 3, 4))
print(coroutine.resume(co, "last", nil))
print(coroutine.status(co), coroutine.resume(co))
print(coroutine.resume(coroutine.running()))
print(select("#", coroutine.running()), coroutine.isyieldable())
print(pcall(coroutine.yield, 1))
print(pcall(coroutine.resume, 1))
print(pcall(coroutine.create, 1))
print(pcall(coroutine.status, nil))
print(pcall(coroutine.wrap, {}))
print(coroutine.resume(coroutine.create(function (...) return select("#", ...), ... end), nil, nil))

-- Errors.
local bad = coroutine.create(function () error("boom") end)
print(coroutine.resume(bad))
print(coroutine.status(bad), coroutine.close(bad))
local bad2 = coroutine.create(function () error({code = 7}) end)
local ok, e = coroutine.resume(bad2)
print(ok, type(e), e.code)
local w = coroutine.wrap(function () error("bad") end)
print(pcall(function () return w() end))
print(pcall(function () local x = w(); return x end))
local w2 = coroutine.wrap(function () local t = nil; return t.x end)
print(pcall(function () local x = w2(); return x end))
local w3 = coroutine.wrap(function () error({}) end)
print(pcall(function () local x = w3(); return type(x) end))

-- Yields across pcall, keeping the protected call.
local pc = coroutine.create(function ()
  local ok, v = pcall(function ()
    local r = coroutine.yield("in pcall")
    error("after " .. r)
  end)
  coroutine.yield(ok, v)
  return "done"
end)
print(coroutine.resume(pc))
print(coroutine.resume(pc, "x"))
print(coroutine.resume(pc))

-- Yields inside metamethods.
local mt = {
  __add = function (a, b) return coroutine.yield("add") end,
  __index = function (t, k) return coroutine.yield("index " .. k) end,
  __newindex = function (t, k, v) coroutine.yield("newindex " .. k) end,
  __lt = function (a, b) return coroutine.yield("lt") end,
  __concat = function (a, b) return coroutine.yield("concat") end,
  __call = function (self, x) return coroutine.yield("call " .. x) end,
  __len = function () return coroutine.yield("len") end,
  __eq = function () return coroutine.yield("eq") end,
}
local m = coroutine.wrap(function ()
  local t, u = setmetatable({}, mt), setmetatable({}, mt)
  local s = t + 1
  local f = t.foo
  t.bar = 1
  local l = t < u
  local c = t .. "x"
  local r = t(5)
  local n = #t
  local q = t == u
  return s, f, l, c, r, n, q
end)
print(m())
print(m(10))
print(m(20))
print(m())
print(m(true))
print(m("cat"))
print(m("called"))
print(m(3))
print(m(false))

-- Yields from iterators and inside generic for bodies.
local function range(n)
  return coroutine.wrap(function () for i = 1, n do coroutine.yield(i, i * i) end end)
end
for a, b in range(3) do print(a, b) end
local outer = coroutine.wrap(function ()
  for i in range(2) do coroutine.yield("outer " .. i) end
  return "outer done"
end)
print(outer(), outer(), outer())

-- Status from inside, and nested coroutines.
local main = coroutine.running()
local parent
parent = coroutine.create(function ()
  print("parent sees", coroutine.status(parent), coroutine.isyieldable())
  local child = coroutine.create(function ()
    print("child sees parent", coroutine.status(parent))
    print("main is", coroutine.status(main))
    coroutine.yield("from child")
  end)
  print(coroutine.resume(child))
  print(coroutine.status(child))
  return "parent done"
end)
print(coroutine.resume(parent))
print(coroutine.status(parent))

-- close.
local s = coroutine.create(function () coroutine.yield() end)
coroutine.resume(s)
print(coroutine.close(s), coroutine.status(s))
print(coroutine.resume(s))
print(pcall(coroutine.close, coroutine.running()))
local selfclose
selfclose = coroutine.create(function () coroutine.close(selfclose); print("not reached") end)
print(coroutine.resume(selfclose))
print(coroutine.status(selfclose))
local norm
norm = coroutine.create(function ()
  local inner = coroutine.create(function () return pcall(coroutine.close, norm) end)
  return coroutine.resume(inner)
end)
print(coroutine.resume(norm))

-- Values through many round trips.
local acc = coroutine.wrap(function (x)
  local sum = 0
  while x do sum = sum + x; x = coroutine.yield(sum) end
  return "total " .. sum
end)
local last
for i = 1, 1000 do last = acc(i) end
print(last, acc(nil))

-- The nesting limit.
local function nest(n)
  if n == 0 then return 0 end
  local c = coroutine.create(function () return nest(n - 1) + 1 end)
  local ok, v = coroutine.resume(c)
  if not ok then error(v, 0) end
  return v
end
print(pcall(nest, 196))
print(pcall(nest, 197))
local function wnest(n)
  if n == 0 then return 0 end
  return coroutine.wrap(function () return wnest(n - 1) + 1 end)()
end
local okw, ew = pcall(wnest, 300)
-- One position prefix per wrap level the error passes through.
print(okw, #ew)
