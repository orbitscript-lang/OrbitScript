local N = tonumber(arg[1])
local t = {}
for i = 1, N do t["k" .. i] = i end
local s = 0
for i = 1, N do s = s + t["k" .. i] end
local acc = ""
for i = 1, N // 50 do acc = acc .. "x" end
print(s + #acc)
