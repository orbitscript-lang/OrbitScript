-- Native closures' upvalues are named "", and a finalizer is the
-- metamethod '__gc'.
local f = string.gmatch("x", "x")
print(debug.getupvalue(f, 1))
print(debug.getupvalue(f, 2))
print(debug.setupvalue(f, 1, "x"))
local name, what
setmetatable({}, {__gc = function ()
  local t = debug.getinfo(1, "n")
  name, what = t.name, t.namewhat
end})
collectgarbage()
print(name, what)
name = nil
setmetatable({}, {__gc = function ()
  local t = debug.getinfo(1, "n")
  name, what = t.name, t.namewhat
end})
local n = 0
repeat local a = {}; n = n + 1 until name or n > 10000000
print(name, what)
