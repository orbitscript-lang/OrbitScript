local N = tonumber(arg[1])
local t = {}
for i = 1, N do t[i] = i end
local s = 0
for r = 1, 10 do for i = 1, N do s = s + t[i] end end
local o = {} o.x = 0
for i = 1, N do o.x = o.x + i end
print(s + o.x)
