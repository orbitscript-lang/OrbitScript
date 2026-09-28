-- Integer/float arithmetic, comparisons and conversions in the VM (no libraries).
local maxi, mini = 9223372036854775807, -9223372036854775807 - 1
print(maxi + 1 == mini, mini - 1 == maxi, maxi * 2, -mini)
print(7 // 2, -7 // 2, 7 // -2, 7 % 3, -7 % 3, 7 % -3, -7 % -3)
print(7.0 // 2, -7.5 // 2, 7 % 2.5, -7 % 2.5, 5.3 % -2, 1e308 * 10, -1e308 * 10)
print(3 / 2, 4 / 2, 2^10, 2^0.5, (-2)^2, 0.0 == -0.0, 1/0 > maxi)
print(3 | 5, 3 & 5, 3 ~ 5, ~0, 1 << 63, 1 << 64, -1 >> 1, 1 >> -1, 2.0 | 1)
print(maxi // -1, mini // -1, mini % -1, pcall(function () return 1 // 0 end))
print(pcall(function () return 1 % 0 end))
print(1 // 0.0, -1 // 0.0, 0/0 ~= 0/0, 5 % (1/0), -5 % (1/0))
print(1 < 1.5, 1 <= 1.0, 2^53 == 2^53 + 1, maxi < 2^63, maxi + 0.0 == 2^63, mini == -2^63)
print(1 == 1.0, "1" == 1, 10 .. 20, 1.5 .. "", -0.0 .. "", 2^63 .. "")
print(pcall(function () return 1 < "2" end))
print(pcall(function () return {} < {} end))
print(pcall(function () return 1.5 | 1 end))
print(pcall(function () return "a" | 1 end))
print(pcall(function () local t = {} return t + 1 end))
print(pcall(function () local s = "x" return -{} end))
print(pcall(function () return #5 end))
print(pcall(function () local a; return a .. "x" end))
print(pcall(function () return 2^63 | 0 end))
for _, v in ipairs({0, 1, -1, 3.0, -3.5, 1e15, 1e16, 123456789012, 0.1, 1/3, 2^63, -2^63}) do
  print(v, v // 1, v * 1.0)
end
print(tonumber("0x10"), tonumber("  12  "), tonumber("1e2"), tonumber("0x1p4"), tonumber("10", 2),
      tonumber("zz", 36), tonumber("8", 8), tonumber(""), tonumber("1 2"), tonumber("0x"), tonumber(nil))
print(tonumber("9223372036854775808"), tonumber("-9223372036854775809"), tonumber("0x7fffffffffffffff"),
      tonumber("0xffffffffffffffff"), tonumber(".5"), tonumber("5."), tonumber("inf"), tonumber("nan"))
local x = 0
for i = 1, 3 do x = x + i end
for i = 3, 1, -1 do x = x + i end
for i = 1.0, 2.0, 0.5 do x = x + i end
for i = maxi - 2, maxi do x = x + 1 end
for i = mini, mini + 2 do x = x + 1 end
for i = 1, 0 do x = x + 1000 end
for i = maxi, maxi - 1 do x = x + 1000 end
print(x)
print(pcall(function () for i = 1, 10, 0 do end end))
print(pcall(function () for i = "a", 10 do end end))
print(pcall(function () for i = 1, {} do end end))
print(pcall(function () for i = 1, 2, "x" do end end))
for i = "1", "3" do io_guard = i end
print(io_guard, math_type_guard)
for i = 1, 3.5 do x = i end print(x)
for i = 3, 0.5, -1 do x = i end print(x)
for i = 1, math_huge_guard or 1e300, 1e299 do x = i end print(x)
