-- Integer and float arithmetic, conversions and edge cases.
-- (No infinities here: inf % x is NaN, whose sign C libraries print differently.)
local vals = {0, 1, -1, 7, -7, 3.5, -3.5, 2^53, math.maxinteger, math.mininteger, 0.1}
for _, a in ipairs(vals) do
  for _, b in ipairs({3, -3, 2.0, 0.5}) do
    print(a, b, a + b, a - b, a * b, a / b, a // b, a % b, math.type(a // b))
  end
end
print(7 // 0.0, -7 // 0.0, 0/0 ~= 0/0, 7 % math.huge, -7 % math.huge, 1/0 + 1, -1/0 * 2)
print(pcall(function () return 1 // 0 end))
print(pcall(function () return 1 % 0 end))
print(math.maxinteger + 1 == math.mininteger, math.mininteger // -1, math.mininteger % -1)
print(3 | 5, 3 & 5, 3 ~ 5, ~0, 1 << 63, 1 << 64, -1 >> 1, 2.0 | 1)
print(pcall(function () return 1.5 | 1 end))
print(10 == 10.0, "10" + 1, "0x10" + 0, "1e2" * 1, 10 .. 20)
print(math.tointeger(3.0), math.tointeger(3.5), math.tointeger("8"), 3 == 3.0000000000000001)
print(tonumber("  0x1p4  "), tonumber("1e"), tonumber("z", 36), tonumber("7fffffffffffffff", 16))
print(string.format("%d %5.2f %g %x %q", 42, math.pi, 1e20, 255, 1/3))
print(math.ult(1, -1), math.abs(math.mininteger), math.fmod(-7, 3), math.fmod(7, -3.0))
