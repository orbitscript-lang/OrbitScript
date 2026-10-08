-- Finalizers run with hooks off: a collection inside hooked code fires no
-- events for the finalizers it runs (db.lua:798 failed under GC stress).
local tr = {}
local function fin()
  local x = 1
  x = x + 1
end
setmetatable({}, {__gc = fin})
debug.sethook(function (e, l) tr[#tr + 1] = e .. ":" .. tostring(l) end, "lcr")
collectgarbage()
debug.sethook()
print(#tr, table.concat(tr, " "))

-- The same in a coroutine with its own hook, count events included.
local co = coroutine.create(function ()
  local t = {}
  setmetatable({}, {__gc = fin})
  debug.sethook(function (e, l) t[#t + 1] = e .. ":" .. tostring(l) end, "lcr", 3)
  collectgarbage()
  debug.sethook()
  return table.concat(t, " ")
end)
print(coroutine.resume(co))

-- A hook still fires after the finalizer returns.
local n = 0
setmetatable({}, {__gc = fin})
debug.sethook(function () n = n + 1 end, "l")
collectgarbage()
local y = 1
y = y + 1
debug.sethook()
print(n)
