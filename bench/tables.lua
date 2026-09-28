-- Hash and array tables: inserts, lookups, deletes, iteration and sort.
local n = tonumber(arg[1]) or 1000000
local arr = {}
for i = 1, n do arr[i] = (i * 7919) % n end
local hash = {}
for i = 1, n do hash[arr[i]] = i end
local hits = 0
for i = 1, n, 3 do if hash[i] then hits = hits + 1 end end
for i = 1, n, 2 do hash[arr[i]] = nil end
local left = 0
for _ in pairs(hash) do left = left + 1 end
local small = {}
for i = 1, n // 10 do small[i] = arr[i] end
table.sort(small)
local strs = {}
for i = 1, n // 10 do strs["key" .. i] = i end
local sum = 0
for i = 1, n // 10 do sum = sum + strs["key" .. i] end
print(hits, left, small[1], small[#small], sum)
