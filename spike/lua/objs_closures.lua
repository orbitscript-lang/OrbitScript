local N = tonumber(arg[1])
local a = {} for i = 1, N do local x = i a[i] = function() return x end end
print(#a)
