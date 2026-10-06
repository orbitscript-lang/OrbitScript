local select, rawequal = select, rawequal
local n = 0
for i = 1, 10000000 do
  if rawequal(select(2, i, i), i) then n = n + 1 end
end
print(n)
