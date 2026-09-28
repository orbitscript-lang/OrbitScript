-- String building, formatting, pattern matching and interning.
local n = tonumber(arg[1]) or 500000
local parts = {}
for i = 1, n do
  parts[#parts + 1] = string.format("%d:%s", i, i % 7 == 0 and "seven" or "x")
end
local s = table.concat(parts, ",")
local sevens = 0
for _ in s:gmatch("%d+:seven") do sevens = sevens + 1 end
local replaced, count = s:gsub("x", "yy")
local keys = {}
for i = 1, n // 10 do keys["k" .. (i % 1000)] = (keys["k" .. (i % 1000)] or 0) + 1 end
local acc = ""
for i = 1, 2000 do acc = acc .. string.char(65 + i % 26) end
print(#s, sevens, #replaced, count, keys.k1, #acc, acc:sub(-5), s:upper():find("SEVEN"))
