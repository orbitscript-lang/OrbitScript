-- __close handlers run by an error, coroutine.close of the running coroutine,
-- and what may yield while closing.
local function mk() return setmetatable({}, {__close=function() print(debug.getinfo(2, "S").what) end}) end
print(pcall(function() local y <close> = mk() error("boom") end))
do local z <close> = mk() end
local co = coroutine.wrap(function() local w <close> = mk(); coroutine.yield() end)
co()
print(pcall(function() local a <close> = mk(); local b <close> = setmetatable({}, {__close=function() error("in b") end}); error("e0") end))
do
print(pcall(coroutine.close))
print(pcall(coroutine.close, coroutine.running()))
local co = coroutine.create(function(...)
  local x <close> = setmetatable({}, {__close=function(_, e) print("closing", e) end})
  print("before")
  local r = table.pack(coroutine.close())
  print("after?", r.n)
end)
print(coroutine.resume(co))
print(coroutine.status(co))
local co2 = coroutine.wrap(function() return pcall(coroutine.close, (coroutine.running())) end)
print(pcall(co2))
local co3 = coroutine.create(function()
  local x <close> = setmetatable({}, {__close=function(_, e) error("in close") end})
  coroutine.close()
end)
print(coroutine.resume(co3))
print(coroutine.status(co3), coroutine.close(co3))
local outer = coroutine.create(function()
  local inner = coroutine.create(function() return coroutine.close(outer) end)
  return coroutine.resume(inner)
end)
print(coroutine.resume(outer))
end
do
local function mk(f) return setmetatable({}, {__close=f}) end
local co = coroutine.create(function() local x <close> = mk(function() print("in close", coroutine.isyieldable()); coroutine.yield() end); coroutine.yield(1) end)
coroutine.resume(co)
print(coroutine.close(co))
local co2 = coroutine.create(function() local x <close> = mk(function() print("self", coroutine.isyieldable()); coroutine.yield() end); coroutine.close() end)
print(coroutine.resume(co2))
local co3 = coroutine.create(function() local x <close> = mk(function() print("err unwind", coroutine.isyieldable()); coroutine.yield(5) end); pcall(error, 1) ; local y <close> = mk(function() print("err2", coroutine.isyieldable()) coroutine.yield(6) end) error("z") end)
print(coroutine.resume(co3))
print(coroutine.resume(co3))
end
do
local function func2close(f) return setmetatable({}, {__close = f}) end
local co
co = coroutine.wrap(function()
  local x <close> = func2close(function() print("inner", pcall(co)) end)
  error(111)
end)
print(pcall(co))
print(pcall(co))
end
