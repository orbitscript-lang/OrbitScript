-- Arithmetic with a numeric literal on either side.
local x, y, f = 7, -3, 2.5
print(x + 1000000, x - 1000000, x * 3, x / 2, x // 2, x % 3, x ^ 2)
print(y // 2, y % 2, y // 2.0, y % 2.0, y % -2, f * 4, f // 1, f % 1)
print(1.5 + x, 10 - x, 3 * x, 1 / x, 10 - f, 2 * f, 0.5 + y)
print(x + 0.5, x - 0.25, x * 1e300 * 1e10, -x * 1e300 * 1e10)
print(x / 0, -x / 0, x % math.huge, -x % math.huge, x // 0.0)
print(math.maxinteger + 1 == math.mininteger, math.mininteger * -1)
print(x ^ 0.5, 2 ^ 0.5)
print(math.type(x * 2), math.type(x * 2.0), math.type(2 * x), math.type(x // 1))
-- Strings coerce.
local s = "12"
print(s + 1.5, s * 2, 3 - s, s // 5, s % 5, s ^ 2, 100 / s)
-- Metamethods see operands in source order.
local mt = {}
for _, e in ipairs { "add", "sub", "mul", "div", "mod", "idiv", "pow" } do
  mt["__" .. e] = function(a, b)
    return e .. "(" .. (type(a) == "table" and "t" or tostring(a)) .. ","
      .. (type(b) == "table" and "t" or tostring(b)) .. ")"
  end
end
local t = setmetatable({}, mt)
print(t + 1.5, t - 2.5, t * 3.5, t / 4.5, t % 5.5, t // 6.5, t ^ 7.5)
print(1.5 + t, 2.5 - t, 3.5 * t, 4.5 / t, 5.5 % t, 6.5 // t, 7.5 ^ t)
print(t + 100000, 100000 - t)
-- Errors name the register operand.
local function try(fn) print(select(2, pcall(fn))) end
local n
try(function() return n + 1.5 end)
try(function() return 1.5 + n end)
try(function() return 2.5 * n end)
try(function() local z = {} return z ^ 2 end)
try(function() return x // 0 end)
try(function() return x % 0 end)
try(function() return "a" + 1.5 end)
try(function() return 1.5 - "b" end)
