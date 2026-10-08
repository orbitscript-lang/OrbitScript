-- Finalizers run with hooks off: no call, return, line or count event
-- fires inside one, whether a collection step or collectgarbage() runs it,
-- and the hook works again afterwards.
local fin = function () local x = 1; x = x + 1 end
local inside, outside = 0, 0
debug.sethook(function ()
  if debug.getinfo(2, "f").func == fin then inside = inside + 1
  else outside = outside + 1 end
end, "lcr", 1)
for _ = 1, 200 do setmetatable({}, {__gc = fin}) end
collectgarbage()
for _ = 1, 2000 do setmetatable({}, {__gc = fin}) end
local hooked = debug.gethook()
debug.sethook()
collectgarbage()
print("events inside finalizers", inside)
print("events outside", outside > 0)
print("hook still set", hooked ~= nil)

-- A finalizer that sets a hook leaves it set, and it fires afterwards.
local seen = 0
setmetatable({}, {__gc = function ()
  debug.sethook(function () seen = seen + 1 end, "l")
end})
collectgarbage()
local _ = 1
debug.sethook()
print("hook set by a finalizer fires", seen > 0)
