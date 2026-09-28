-- Allocation-heavy: builds and walks many short-lived binary trees (GC).
local function bottom_up(depth)
  if depth == 0 then return {} end
  depth = depth - 1
  return {bottom_up(depth), bottom_up(depth)}
end

local function check(tree)
  if tree[1] then return 1 + check(tree[1]) + check(tree[2]) end
  return 1
end

local n = tonumber(arg[1]) or 16
local mindepth = 4
local maxdepth = math.max(mindepth + 2, n)

print("stretch tree of depth " .. maxdepth + 1 .. "\t check: " .. check(bottom_up(maxdepth + 1)))
local long_lived = bottom_up(maxdepth)
for depth = mindepth, maxdepth, 2 do
  local iterations = 2 ^ (maxdepth - depth + mindepth) // 1
  local sum = 0
  for _ = 1, iterations do sum = sum + check(bottom_up(depth)) end
  print(string.format("%d\t trees of depth %d\t check: %d", iterations, depth, sum))
end
print("long lived tree of depth " .. maxdepth .. "\t check: " .. check(long_lived))
