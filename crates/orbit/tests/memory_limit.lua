-- Run with a 16 MiB cap (memory_limit.rs).

-- Garbage-heavy work under the cap: collections keep it going.
local s = 0
for i = 1, 200000 do local t = {i, i + 1, tostring(i)}; s = s + #t end
assert(s == 600000)

-- Growing a live table past the cap raises a catchable error, and the
-- handler has room to run.
local function grow(t) for i = 1, math.huge do t[i] = {} end end
local a = {}
local ok, msg = pcall(grow, a)
assert(not ok and msg == "not enough memory" and #a > 1000)
local note = string.format("stopped at %d", #a)
assert(#note > 0)
a = nil
collectgarbage()
assert(collectgarbage("count") < 1024)

-- Large allocations are refused before they happen.
assert(select(2, pcall(string.rep, "x", 1 << 26)) == "not enough memory")
local big = string.rep("y", 1 << 20)
assert(select(2, pcall(function () return big .. string.rep("z", 1 << 23) .. big end))
  == "not enough memory")
if table.create then   -- Lua 5.5 only
  assert(select(2, pcall(table.create, 1 << 24)) == "not enough memory")
end
assert(collectgarbage("count") < 16 * 1024)

-- Holding on past the cap: the error comes back once the grace is used.
local keep = {}
assert(not pcall(grow, keep))
assert(not pcall(grow, keep))
keep = nil
collectgarbage()

-- The error is raised at a safe point of the loop that is running: the
-- same under a count hook and a line hook, which run the other loop.
local NOMEM = "not enough memory"
local hooked = 0
for _, how in ipairs{{"", 100}, {"l"}} do
  local before = hooked
  debug.sethook(function() hooked = hooked + 1 end, how[1], how[2])
  local held = {}
  local ok, msg = pcall(grow, held)
  debug.sethook()
  assert(not ok and msg == NOMEM and #held > 1000, msg)
  assert(hooked > before)
end
collectgarbage()

-- Setting a hook from inside the growing loop moves it to the other loop
-- at the next safe point; the limit is found there all the same.
do
  local held = {}
  local ok, msg = pcall(function()
    for i = 1, math.huge do
      held[i] = {}
      if i == 1000 then debug.sethook(function() end, "", 1000) end
    end
  end)
  debug.sethook()
  assert(not ok and msg == NOMEM and #held > 1000, msg)
end
collectgarbage()

-- Inside the grace: a list grows by small allocations only, so the limit
-- is found at a safe point. Right after the error the heap is still over
-- the cap and the script goes on making small values (anything sized up
-- front, such as a growing table, is refused); only growing past the
-- allowance raises again. Dropping the data ends the grace, and the next
-- time is a first time again.
do
  local cap = 16 * 1024
  local list
  local function chain() for i = 1, math.huge do list = {list} end end
  local ok, msg = pcall(chain)
  assert(not ok and msg == NOMEM)
  assert(collectgarbage("count") > cap)
  local n = 0
  for i = 1, 10000 do local small = {i, tostring(i)}; n = n + #small end
  assert(n == 20000 and collectgarbage("count") > cap)
  ok, msg = pcall(chain)
  assert(not ok and msg == NOMEM)
  assert(collectgarbage("count") > cap + cap // 8)
  list = nil
  collectgarbage()
  assert(collectgarbage("count") < cap // 2)
  ok, msg = pcall(chain)
  assert(not ok and msg == NOMEM)
  local at = collectgarbage("count")
  assert(at > cap and at < cap + cap // 8)
  for i = 1, 10000 do local small = {i}; n = n + #small end
  assert(n == 30000)
  list = nil
end
collectgarbage()

-- In a coroutine the error unwinds it and reaches the resumer as any
-- other error does; a message handler runs before anything unwinds.
do
  local held = {}
  local co = coroutine.create(grow)
  local ok, msg = coroutine.resume(co, held)
  assert(not ok and msg == NOMEM and coroutine.status(co) == "dead")
  -- A coroutine that died with an error keeps its stack until closed.
  ok, msg = coroutine.close(co)
  assert(not ok and msg == NOMEM)
  co = nil
  held = {}
  collectgarbage()
  local handled
  ok, msg = xpcall(grow, function(m) handled = m; return #held end, held)
  assert(not ok and handled == NOMEM and msg == #held and msg > 1000)
  -- The error comes at a safe point of the loop, and the frame knows
  -- where it was: the dead coroutine still shows the line.
  held = nil
  collectgarbage()
  local list
  -- Released after the error, so there is room to ask.
  local pad = string.rep("x", 4 << 20)
  local here = debug.getinfo(1, "l").currentline
  co = coroutine.create(function()
    for i = 1, math.huge do
      list = {list}
    end
  end)
  ok, msg = coroutine.resume(co)
  assert(not ok and msg == NOMEM)
  pad = nil
  collectgarbage()
  local line = debug.getinfo(co, 0, "l").currentline
  assert(line == here + 3, line)
  list = nil
  coroutine.close(co)
end
collectgarbage()
return "done"
