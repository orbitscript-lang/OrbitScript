-- Coroutine chains longer than the hot stack set: primes below 1000 by a
-- chain of filter coroutines, one per prime found (168 at the end), each
-- resuming the one below it. Run arg[1] times.
local rounds = tonumber(arg[1]) or 500
local function numbers(limit)
  return coroutine.wrap(function ()
    for i = 2, limit do coroutine.yield(i) end
  end)
end
local function without_multiples(p, src)
  return coroutine.wrap(function ()
    while true do
      local v = src()
      if v == nil then return end
      if v % p ~= 0 then coroutine.yield(v) end
    end
  end)
end
local total = 0
for _ = 1, rounds do
  local src = numbers(1000)
  while true do
    local p = src()
    if p == nil then break end
    total = total + p
    src = without_multiples(p, src)
  end
end
print(total)
