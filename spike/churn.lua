local peak = 0
for i = 1, 1000000 do
  local co = coroutine.create(function () coroutine.yield() end)
  coroutine.resume(co)
  if i % 2 == 0 then coroutine.resume(co) end
  if i % 100000 == 0 then
    local k = collectgarbage("count")
    if k > peak then peak = k end
  end
end
collectgarbage()
print(peak < 20000, math.floor(collectgarbage("count")))
