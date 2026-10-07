-- __gc: registration, order, resurrection, weak tables seen from a
-- finalizer. Warnings stay off, so errors in finalizers print nothing.
-- Objects are held until an explicit collection, so the output doesn't
-- depend on when automatic collection runs.
collectgarbage("incremental")
local hold
local log = {}
local function flush(label)
  print(label, table.concat(log, " "))
  log = {}
end

local mt = {__gc = function (o) log[#log + 1] = o.n end}
hold = {}
for i = 1, 5 do hold[i] = setmetatable({n = i}, mt) end
hold = nil
collectgarbage()
flush("newest first")

local objs = {}
for i = 1, 5 do objs[i] = {n = i} end
for _, i in ipairs{3, 1, 5, 2, 4} do setmetatable(objs[i], mt) end
objs = nil
collectgarbage()
flush("registration order")

-- __gc added to the metatable after setmetatable: not registered.
local m2 = {}
local late = setmetatable({n = "late"}, m2)
m2.__gc = mt.__gc
late = nil
collectgarbage()
flush("late (none)")

-- setmetatable again registers it.
local m3 = {}
local again = setmetatable({n = "again"}, m3)
m3.__gc = mt.__gc
setmetatable(again, m3)
again = nil
collectgarbage()
flush("again")

-- __gc removed before the object dies: nothing is called. false and
-- non-callable values register but fail quietly.
local m4 = {__gc = mt.__gc}
local gone = setmetatable({n = "gone"}, m4)
m4.__gc = nil
gone = nil
hold = {
  setmetatable({}, {__gc = false}),
  setmetatable({}, {__gc = 42}),
  setmetatable({n = "after errors"}, mt),
}
hold = nil
collectgarbage()
flush("removed")

-- Errors in a finalizer don't stop the others.
hold = {
  setmetatable({n = 1}, mt),
  setmetatable({}, {__gc = function () error("boom") end}),
  setmetatable({n = 3}, mt),
}
hold = nil
collectgarbage()
flush("errors")

-- Resurrection, and re-registration from the finalizer.
local keep
local m5 = {}
m5.__gc = function (o)
  log[#log + 1] = "fin" .. o.n
  if o.n < 3 then o.n = o.n + 1; setmetatable(o, m5) end
  keep = o
end
hold = setmetatable({n = 1, child = {"child"}}, m5)
hold = nil
collectgarbage()
flush("resurrect " .. keep.n .. " " .. keep.child[1])
for _ = 1, 3 do keep = nil; collectgarbage() end
flush("re-registered " .. tostring(keep))

-- An object reachable only from finalized objects survives for them.
local order = {}
local mt2 = {__gc = function (o) order[#order + 1] = o.n .. (o.ref and o.ref.n or "") end}
local a1 = setmetatable({n = 1}, mt2)
local a2 = setmetatable({n = 2, ref = a1}, mt2)
a1, a2 = nil, nil
collectgarbage()
print("references", table.concat(order, ","))

-- Weak tables seen from a finalizer: weak values are already cleared,
-- weak keys are still there and go one cycle later.
local wk = setmetatable({}, {__mode = "k"})
local wv = setmetatable({}, {__mode = "v"})
local wkv = setmetatable({}, {__mode = "kv"})
local obj = setmetatable({}, {__gc = function (o)
  print("in gc", wk[o], wv[1], wkv[o], wkv[2])
end})
wk[obj] = "k"; wv[1] = obj; wkv[obj] = "x"; wkv[2] = obj
obj = nil
collectgarbage()
print("after 1", next(wk) ~= nil, next(wv), next(wkv) ~= nil)
collectgarbage()
print("after 2", next(wk), next(wv), next(wkv))

-- gc.lua's 5.1 bug: a weak-value entry is gone and a weak-key entry is
-- still there when the finalizer runs.
do
  local a = {}
  local t = {x = 10}
  local C = setmetatable({key = t}, {__mode = "v"})
  local C1 = setmetatable({[t] = 1}, {__mode = "k"})
  a.x = t
  setmetatable(a, {__gc = function (u)
    print("5.1 bug", C.key, type(next(C1)))
  end})
  a, t = nil, nil
  collectgarbage()
  collectgarbage()
  print("5.1 bug after", next(C), next(C1))
end

-- The finalizer's own metatable entries: a weak __gc is collected before
-- it could run, and a weak table reached only from the object is cleared.
local u = setmetatable({}, {__gc = true})
setmetatable(getmetatable(u), {__mode = "v"})
getmetatable(u).__gc = function () print("cannot happen") end
u = nil
collectgarbage()
local u2 = setmetatable({}, {__gc = true})
local m6 = getmetatable(u2)
m6.x = setmetatable({[{0}] = 1, [0] = {1}}, {__mode = "kv"})
m6.__gc = function (o) print("own weak table empty", next(getmetatable(o).x)) end
u2, m6 = nil, nil
collectgarbage()

-- Finalizers run during allocation, without an explicit collection.
local fired = false
setmetatable({}, {__gc = function () fired = true end})
local i = 0
repeat local t = {}; i = i + 1 until fired or i > 10000000
print("automatic", fired)

-- A finalizer that allocates and creates new finalizable objects.
local made = 0
local mt7 = {}
mt7.__gc = function (o)
  made = made + 1
  local junk = {}
  for j = 1, 100 do junk[j] = {j} end
  if o.n < 5 then setmetatable({n = o.n + 1}, mt7) end
end
setmetatable({n = 1}, mt7)
for _ = 1, 6 do collectgarbage() end
print("chained", made)
