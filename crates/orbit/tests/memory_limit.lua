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
assert(select(2, pcall(table.create, 1 << 24)) == "not enough memory")
assert(collectgarbage("count") < 16 * 1024)

-- Holding on past the cap: the error comes back once the grace is used.
local keep = {}
assert(not pcall(grow, keep))
assert(not pcall(grow, keep))
return "done"
