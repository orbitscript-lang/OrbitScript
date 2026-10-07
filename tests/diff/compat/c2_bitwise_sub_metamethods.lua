-- Bitwise metamethods (__band .. __bnot) and subtraction of an integer constant: __sub sees
-- the constant as written, and -0.0 - 0 is 0.0.
local mt = {}
for _, e in ipairs{"band","bor","bxor","shl","shr","bnot"} do
  mt["__" .. e] = function(a, b) return e .. ":" .. tostring(a == nil) .. ":" .. (rawequal(a, b) and "same" or "diff") end
end
local t = setmetatable({}, mt)
print(t & 1, 1 | t, t ~ t, t << 2, 3 >> t, ~t)
print(pcall(function() return 1.5 | t end))
print(pcall(function() return t | 1.5 end))
print(pcall(function() return 1.5 | {} end))
print(pcall(function() return {} | 1.5 end))
print(pcall(function() return 1.5 | 2 end))
print(pcall(function() return "3" | 1 end))
print(pcall(function() return ~"3" end))
print(pcall(function() return ~1.5 end))
local s = setmetatable({}, {__band = function(a, b) return "s" end})
print(t & s, s & t)
local mt2 = {__sub = function(a, b) return b end, __add = function(a, b) return b end}
local u = setmetatable({}, mt2)
print(u - 3, u - 0, u - -5, u + 3, u - 40000, type(3 - u))
print(("10") - 3, ("1.5") - 1, -0.0 - 0, 0.0 - 0, 5 - 0, math.mininteger - 1, 2.5 - 1)
print(pcall(function() return {} - 3 end))
print(pcall(function() return "x" - 3 end))
do
local z = -0.0
print(z - 0, z + 0, z - 0.0, -0.0 - 0, z - 1 + 1, 0 - z)
local k <const> = 0
print(z - k)
local a, b = -0.0, 0
print(a - b, -0.0 - 0, (-0.0) - 0, 0.0 - 0, -0.0 - 0.0, -(0.0) - 0, -0.0 - 1 + 1, -0.0 + 0)
local c2 <const> = -0.0
print(c2 - 0, c2 + 0, -0.0 - -0, -0.0 * 1)
end
