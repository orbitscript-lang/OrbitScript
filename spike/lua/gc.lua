local N = tonumber(arg[1])
local ring, keep = {}, 0
for i = 1, N do
  local t = {i, i + 1, x = i}
  local f = function() return t end
  ring[i % 1000 + 1] = f
  local c = {} c.self = c
  keep = keep + #t
end
print(keep)
