-- `__call` chains: 15 levels work, 16 are too long.
local a = {}
for i = 1, 15 do a = setmetatable({}, {__call = a}) end
print(pcall(a))
local a = {}
for i = 1, 16 do a = setmetatable({}, {__call = a}) end
print(pcall(a))
setmetatable(a, {__call = a})
print(pcall(a))
print(pcall(function () return a() end))
local b = {}
for i = 1, 15 do b = setmetatable({}, {__call = b}) end
print(pcall(function () return b() end))
