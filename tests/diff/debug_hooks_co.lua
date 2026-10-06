-- Hooks in coroutines: a Lua hook can't yield, and isyieldable says so.
local co = coroutine.create(function ()
  debug.sethook(function () coroutine.yield(1) end, "l")
  local x = 1
  return x
end)
print(coroutine.resume(co))
print(coroutine.status(co))
local co2 = coroutine.wrap(function ()
  debug.sethook(function () print(coroutine.isyieldable()) debug.sethook() end, "l")
  local y = 2
end)
co2()
