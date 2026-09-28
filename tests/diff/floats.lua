-- Lua 5.5 prints floats with enough digits to round-trip.
local xs = {0.1, 1/3, 2/3, 100.0, 1e15, 1e16, 1e100, -0.0, 123456789012345.6,
            2^63, -2^63, 5e-324, 1.7976931348623157e308, 3.14159, 1e-7}
for _, x in ipairs(xs) do
  print(x, tostring(x), x == tonumber(tostring(x)), string.format("%.14g", x))
end
print(1e308 * 10, -1e308 * 10, math.floor(3.7), math.ceil(-3.7), math.floor(-0.0))
print(string.format("%a", 1.0), 3 / 2, 4 / 2, 2^2, 7 // 2.0)
