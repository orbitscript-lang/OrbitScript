-- First-light subset: garbage collection keeps memory bounded.
local function make(depth)
  if depth == 0 then return {} end
  return {make(depth - 1), make(depth - 1)}
end
local keep = make(10)
local before = collectgarbage("count")
for round = 1, 200 do
  local tmp = make(8)
  local s = ""
  for k = 1, 20 do s = s .. k end
end
collectgarbage()
local after = collectgarbage("count")
print(type(before), after < before * 4, #keep)
local cycle = {}
cycle.self = cycle
cycle = nil
collectgarbage("collect")
print("done")
