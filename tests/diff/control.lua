-- Control flow: loops, goto, numeric-for edge cases.
for i = 1, 3 do io.write(i, " ") end
for i = 3, 1, -1 do io.write(i, " ") end
for i = 1.0, 2.0, 0.5 do io.write(i, " ") end
for i = math.maxinteger - 1, math.maxinteger do io.write(i, " ") end
print()
print(pcall(function () for i = 1, 10, 0 do end end))
local i = 0
repeat local j = i; i = i + 1 until j >= 3
print(i)
while true do i = i + 1; if i > 6 then break end end
print(i)
for k = 1, 3 do
  for m = 1, 3 do
    if m == 2 then goto continue end
    io.write(k, m, " ")
    ::continue::
  end
end
print()
do goto skip; print("not printed"); ::skip:: end
print(1 and 2, nil and 1, false or "x", nil or false, not nil, 1 == 1.0 and "eq")
