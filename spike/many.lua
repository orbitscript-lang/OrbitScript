-- 100 000 suspended coroutines, each yielded at depth arg[2].
local n = tonumber(arg[1]) or 100000
local depth = tonumber(arg[2]) or 0
local function down(d)
  if d == 0 then return coroutine.yield() end
  return (down(d - 1))
end
local cos = {}
for i = 1, n do
  local co = coroutine.create(function () down(depth) end)
  coroutine.resume(co)
  cos[i] = co
end
collectgarbage()
print(#cos, math.floor(collectgarbage("count")))
