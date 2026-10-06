-- Round trips with the yield 20 Lua frames below the coroutine body.
local n = tonumber(arg[1]) or 1000000
local function down(d, i)
  if d == 0 then return coroutine.yield(i) end
  return (down(d - 1, i))
end
local gen = coroutine.wrap(function ()
  for i = 1, n do down(20, i) end
  return nil
end)
local sum = 0
while true do
  local v = gen()
  if v == nil then break end
  sum = sum + v
end
print(sum)
