-- Coroutine switches: a producer/consumer pipeline.
local n = tonumber(arg[1]) or 4000000
local function producer()
  return coroutine.wrap(function ()
    for i = 1, n do coroutine.yield(i) end
    coroutine.yield(nil)
  end)
end
local function filter(src)
  return coroutine.wrap(function ()
    while true do
      local v = src()
      if v == nil then coroutine.yield(nil) return end
      if v % 3 ~= 0 then coroutine.yield(v * 2) end
    end
  end)
end
local sum, count = 0, 0
local src = filter(producer())
while true do
  local v = src()
  if v == nil then break end
  sum = sum + v
  count = count + 1
end
print(count, sum)
