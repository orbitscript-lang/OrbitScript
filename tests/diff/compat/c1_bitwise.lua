-- Bitwise operators on integers, floats with integer values, shifts and errors.
print(7 & 3, 7 | 8, 7 ~ 2, ~0, ~5, 1 << 2, 256 >> 4, -1 >> 60, -1 << 63)
print(1 << 63, 1 << 64, 1 >> 64, 1 << -1, 2 >> -1, -1 >> 1, 1 << -64)
print(3.0 & 1, 2^53 | 0, 0xF0 ~ 0xFF, ~-1)
print(math.maxinteger & math.mininteger, math.mininteger | 1, math.maxinteger ~ -1)
-- Precedence: shifts bind tighter than &, & than ~, ~ than |; comparisons are lower.
print(1 | 2 ~ 3 & 4 << 1, 1 + 1 << 2, 5 & 3 == 1, -1 >> 1 + 1)
local a, b = 0xAA, 0x55
print(a & b, a | b, a ~ b, ~a & 0xFF, a >> 1, b << 1)
local acc = "" for i = 0, 4 do acc = acc .. (1 << i) .. " " end print(acc)
local function e(f) local ok, m = pcall(f) print(ok, m) end
local t, s, x = {}, "3", 1.5
e(function() return 1 & 1.5 end)
e(function() return x | 1 end)
e(function() return t & 1 end)
e(function() return 1 ~ t end)
e(function() return ~t end)
e(function() return s << 1 end)
e(function() return nil >> 1 end)
e(function() return undefined_global & 1 end)
e(function() return t.field | 1 end)
e(function() return math.huge | 0 end)
e(function() return (0/0) | 0 end)
e(function() local y = 2^63 return y | 1 end)
