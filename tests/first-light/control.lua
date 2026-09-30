-- First-light subset: if, while, repeat, numeric and generic for, break.
local function classify(n)
  if n < 0 then return "negative" elseif n == 0 then return "zero" elseif n < 10 then return "small" else return "large" end
end
print(classify(-5), classify(0), classify(3), classify(99))

local i, total = 0, 0
while true do
  i = i + 1
  if i > 100 then break end
  if i % 2 == 0 then total = total + i end
end
print(i, total)

local n = 0
repeat
  local sq = n * n
  n = n + 1
until sq > 50
print(n)

local parts = {}
for k = 10, 1, -3 do parts[#parts + 1] = k end
for k = 1, 2, 0.5 do parts[#parts + 1] = k end
for k = 1, 0 do parts[#parts + 1] = "never" end
for k = math.maxinteger - 2, math.maxinteger do parts[#parts + 1] = k - math.maxinteger end
print(table.concat(parts, " "))

local limit = 3.5
local count = 0
for k = 1, limit do count = count + k end
print(count)

-- nested loops with break
local found
for a = 1, 10 do
  for b = 1, 10 do
    if a * b == 42 then found = a .. "x" .. b; break end
  end
  if found then break end
end
print(found)

-- generic for with a hand-written iterator and with ipairs
local function range(n)
  local function step(limit, cur)
    if cur < limit then return cur + 1, (cur + 1) * 2 end
  end
  return step, n, 0
end
for v, twice in range(4) do print(v, twice) end
local sum = 0
for idx, v in ipairs({5, 6, 7, nil, 9}) do sum = sum + idx * v end
print(sum)
local keys, vals = 0, 0
for key, v in pairs({a = 1, b = 2, c = 3, 10, 20}) do keys = keys + 1; vals = vals + v end
print(keys, vals)
