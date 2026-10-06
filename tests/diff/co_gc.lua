-- Values left above a parked stack's top are garbage: the main thread's
-- while a coroutine runs, and a suspended coroutine's.
local function big() local t = {} for i = 1, 200000 do t[i] = i end return t end
local function deep(n) if n == 0 then local t = big(); return #t end return deep(n - 1) + 0 end
collectgarbage(); local base = collectgarbage("count")
deep(50)
local co = coroutine.wrap(function() collectgarbage(); collectgarbage(); return collectgarbage("count") end)
print("main parked:", co() - base < 1000)
local co2 = coroutine.create(function() deep(50); coroutine.yield() end)
coroutine.resume(co2)
collectgarbage(); collectgarbage()
print("suspended:", collectgarbage("count") - base < 1000)
