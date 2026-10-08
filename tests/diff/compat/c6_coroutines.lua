-- A dead coroutine keeps its hook. One that died with an error keeps its
-- stack for tracebacks and locals until it is closed.
local foo = function() end
local co = coroutine.create(function() return 1 end)
debug.sethook(co, foo, "l")
coroutine.resume(co)
print(coroutine.status(co), debug.gethook(co) == foo, select(2, debug.gethook(co)))
local co2 = coroutine.create(function() error("x") end)
debug.sethook(co2, foo, "c", 3)
coroutine.resume(co2)
print(coroutine.status(co2), debug.gethook(co2) == foo, select(2, debug.gethook(co2)))

local function f(i) if i == 0 then error(i) else coroutine.yield(); f(i - 1) end end
local co3 = coroutine.create(function (x) f(x) end)
coroutine.resume(co3, 2)
while coroutine.status(co3) == "suspended" do print(coroutine.resume(co3)) end
print(debug.traceback(co3))
print(debug.traceback(co3, "msg", 1))
print(coroutine.close(co3))
print(debug.traceback(co3))

local co4 = coroutine.create(function () local x = nil; local y = 7; x() end)
print(coroutine.resume(co4))
print(debug.getlocal(co4, 0, 1), debug.getlocal(co4, 0, 2))
print(debug.getinfo(co4, 0, "l").currentline)
print(coroutine.close(co4))
print(debug.getinfo(co4, 0))
