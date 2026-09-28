-- Coroutines: yields across pcall, metamethods and iterators; wrap, close, status.
local function show(...) print(...) return ... end
-- yield across pcall/xpcall inside a coroutine
local co = coroutine.create(function ()
  local ok, v = pcall(function ()
    local x = coroutine.yield("in pcall")
    error("after " .. x, 0)
  end)
  show("pcall returned", ok, v)
  local ok2, v2 = xpcall(function () return coroutine.yield("in xpcall") end, function (m) return "h:" .. m end)
  show("xpcall returned", ok2, v2)
  return "end"
end)
show(coroutine.resume(co))
show(coroutine.resume(co, "resume1"))
show(coroutine.resume(co, "resume2"))
show(coroutine.status(co), coroutine.resume(co))
-- yield from inside a metamethod
local mt = {__index = function (t, k) return coroutine.yield("index " .. k) end,
            __add = function (a, b) return coroutine.yield("add") end,
            __lt = function (a, b) return coroutine.yield("lt") end,
            __concat = function (a, b) return coroutine.yield("concat") end,
            __eq = function (a, b) return coroutine.yield("eq") end,
            __len = function (a) return coroutine.yield("len") end,
            __newindex = function (t, k, v) coroutine.yield("newindex " .. k) end,
            __call = function (self, x) return coroutine.yield("call " .. x) end}
local obj = setmetatable({}, mt)
local obj2 = setmetatable({}, mt)
local c2 = coroutine.wrap(function ()
  local r = {}
  r[1] = obj.foo
  r[2] = obj + 1
  r[3] = obj < obj2
  r[4] = "a" .. obj .. "b"
  r[5] = obj == obj2
  r[6] = #obj
  obj.bar = 1
  r[7] = obj("x")
  if obj < obj2 then r[8] = "lt-true" else r[8] = "lt-false" end
  return r
end)
local v = c2()
local answers = {"FOO", 41, true, "CAT", false, 7, nil, "CALLED", false}
local i = 1
while type(v) ~= "table" do
  print("yielded", v)
  v = c2(answers[i])
  i = i + 1
end
print(v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8])
-- yield from a for-in iterator
local function iter() return coroutine.wrap(function () for k = 1, 3 do coroutine.yield(k, k * k) end end) end
for a, b in iter() do print("iter", a, b) end
-- errors
print(pcall(coroutine.yield, 1))
print(coroutine.resume(coroutine.running()))
local dead = coroutine.create(function () return 1 end)
coroutine.resume(dead)
print(coroutine.resume(dead))
print(pcall(coroutine.wrap(function () error("wrapped") end)))
print(pcall(coroutine.wrap(function () error({}) end)))
local w = coroutine.wrap(function () end)
w()
print(pcall(w))
print(pcall(coroutine.create, 1))
print(pcall(coroutine.resume, 1))
print(pcall(coroutine.status, nil))
-- status transitions
local outer
outer = coroutine.create(function ()
  local inner = coroutine.create(function () print("inner sees outer", coroutine.status(outer)) end)
  coroutine.resume(inner)
  print("outer sees itself", coroutine.status(outer), coroutine.isyieldable())
end)
print(coroutine.status(outer))
coroutine.resume(outer)
print(coroutine.status(outer), coroutine.isyieldable())
-- resume passes values both ways, including nils
local echo = coroutine.create(function (...)
  local n = select("#", ...)
  while true do n = select("#", coroutine.yield(n)) end
end)
print(coroutine.resume(echo, nil, nil))
print(coroutine.resume(echo))
print(coroutine.resume(echo, 1, nil, 3, nil))
-- close with pending to-be-closed variables and after errors
local cc = coroutine.create(function ()
  local x <close> = setmetatable({}, {__close = function (_, e) print("closing x", e) end})
  coroutine.yield()
end)
coroutine.resume(cc)
print(coroutine.close(cc), coroutine.status(cc))
local ce = coroutine.create(function ()
  local x <close> = setmetatable({}, {__close = function (_, e) print("closing after error", e) end})
  error("fail", 0)
end)
print(coroutine.resume(ce))
print(coroutine.close(ce))
print(coroutine.close(coroutine.create(print)))
print(pcall(coroutine.close, coroutine.running()))
-- wrap closes to-be-closed variables of a failed coroutine
local wf = coroutine.wrap(function ()
  local x <close> = setmetatable({}, {__close = function () print("wrap closed") end})
  error("wrap fail", 0)
end)
print(pcall(wf))
-- deep recursion of resumes
local function nest(n)
  if n == 0 then return 0 end
  local c = coroutine.create(nest)
  local ok, v = coroutine.resume(c, n - 1)
  if not ok then error(v, 0) end
  return v + 1
end
print(nest(50))
print(pcall(nest, 300))
