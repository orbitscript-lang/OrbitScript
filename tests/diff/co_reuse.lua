-- New coroutines run on buffers left by finished ones and by suspended
-- ones whose stacks went cold: each must start clean, and the cold ones
-- must still resume with their own values.
local function body(tag)
  local a, b, c
  local t = {tag, a, b, c}
  coroutine.yield(#t, select("#", a, b, c))
  return tag .. "!"
end

-- Finished coroutines, back to back.
local out = {}
for i = 1, 5 do
  local co = coroutine.wrap(function(x, ...) local y; return x + select("#", ...), y end)
  local r, y = co(i, nil, nil)
  out[#out + 1] = r .. ":" .. tostring(y)
end
print(table.concat(out, " "))

-- Suspended coroutines, enough that the oldest go cold and their stacks
-- shrink, each holding a table only it refers to.
local cos = {}
for i = 1, 40 do
  local co = coroutine.create(function(n)
    local mine = {n}
    local again = coroutine.yield(n)
    return mine[1] + again
  end)
  assert(coroutine.resume(co, i))
  cos[i] = co
end
collectgarbage()
-- New coroutines reuse the cold stacks' buffers.
for i = 1, 10 do
  local co = coroutine.wrap(body)
  local n, m = co("t" .. i)
  assert(n == 1 and m == 3, n)
  assert(co() == "t" .. i .. "!")
end
local sum = 0
for i = 1, 40 do
  local ok, v = coroutine.resume(cos[i], 1000)
  assert(ok, v)
  sum = sum + v
end
print("cold resumed:", sum)

-- A coroutine that died with an error keeps its stack for the debug
-- library; one with a deep stack is too big to keep.
local co = coroutine.create(function() local x = "kept"; error("boom") end)
print(coroutine.resume(co))
print(debug.getlocal(co, 1, 1))
local function deep(n) if n == 0 then return 0 end return 1 + deep(n - 1) end
print(coroutine.wrap(function() return deep(5000) end)())
print(coroutine.wrap(function(...) local a, b, c; return select("#", ...), a, b, c end)(1, 2))
