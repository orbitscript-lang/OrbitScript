-- string.dump and loading binary chunks. The format is our own, so only
-- behavior is compared, never the bytes.

local function roundtrip(f, strip)
  return assert(load(string.dump(f, strip)))
end

-- Functions keep working after a round trip.
local function sum(n)
  local t = {}
  for i = 1, n do t[i] = i * 2 end
  local s = 0
  for _, v in ipairs(t) do s = s + v end
  return s, #t
end
print(roundtrip(sum)(10))
print(roundtrip(sum, true)(10))
print(roundtrip(function(...) return select("#", ...), ... end)(1, nil, 3))
print(roundtrip(function()
  local x = 0
  local function inc() x = x + 1 return x end
  inc()
  return inc(), string.rep("a", 3), 2^10, 7 // 2, -0.0, math.maxinteger
end)())
print(roundtrip(function(a, b)
  local r = {}
  for i = 10, 1, -3 do r[#r + 1] = i end
  for i = 1.0, 2.0, 0.5 do r[#r + 1] = i end
  while a < b do a = a + 1 end
  repeat b = b - 1 until b < 0
  return table.concat(r, ","), a, b, a == 5 and "yes" or "no"
end)(1, 5))

-- Error positions come from the dumped chunk's name; stripped chunks have
-- no line information, so messages carry no position.
print(pcall(load(string.dump(load("error('x')", "=hello")), "=other")))
print(pcall(load(string.dump(load("error('x')", "=hello"), true), "=other")))
print(pcall(load(string.dump(load("local t = nil; return t.x", "=idx")))))

-- Captures: the first gets the environment, the rest start as nil.
local g = roundtrip(load("local a, b = 1, 2 return function() return a, b end")())
local a, b = g()
print(a == _G, b)
local up = 10
local h = roundtrip(function() return up end)
print(h() == _G)

-- Globals in a loaded binary chunk resolve through the environment given.
local code = string.dump(load("return x, y"))
print(load(code, "c", "b", {x = 1, y = "two"})())

-- Bad input.
print(pcall(string.dump, print))
print(pcall(string.dump))
print(pcall(string.dump, 1))
print(load(string.dump(load("return 1")), "x", "t"))
print(load("return 1", "x", "b"))
local d = string.dump(sum)
local truncated = 0
for i = 1, #d - 1 do
  local f, msg = load(string.sub(d, 1, i))
  if not f and string.sub(msg, -17) == "(truncated chunk)" then
    truncated = truncated + 1
  end
end
print(truncated == #d - 1)
print(load(string.sub(d, 1, 3)))
print(load(string.sub(d, 1, 3), "=bin"))
print(load("\27Luaxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx") == nil)

-- A dumped chunk dumps again.
local twice = roundtrip(roundtrip(sum))
print(twice(4))
print(#string.dump(sum, true) < #string.dump(sum))
