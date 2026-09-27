-- Tables: constructors, length, the table library, sorted iteration.
local t = {10, 20, 30, nil, 50, x = 1, ["y z"] = 2, [3.0] = "three"}
print(t[3], t[3.0], #{1, 2, 3}, rawlen({1, 2}), select("#", table.unpack({1, nil, 3}, 1, 3)))
local keys = {}
for k in pairs({a = 1, b = 2, c = 3, 1, 2}) do keys[#keys + 1] = tostring(k) end
table.sort(keys)
print(table.concat(keys, " "))
local l = {}
for i = 1, 10 do table.insert(l, i * i) end
table.insert(l, 1, 0)
print(table.remove(l), table.remove(l, 1), #l, table.concat(l, ","))
print(table.concat(table.move({1, 2, 3}, 1, 3, 2), ","), table.unpack({1, 2, 3}, 2))
local words = {"pear", "Apple", "fig", "banana", "cherry"}
table.sort(words, function (a, b) return a:lower() < b:lower() end)
print(table.concat(words, " "))
print(pcall(table.insert, {}, 5, 1), pcall(table.concat, {{}}))
local c = table.create(4, 2)
print(type(c), #c, next(c))
print(next({}), type(next), rawequal(t, t), rawget(t, "x"))
