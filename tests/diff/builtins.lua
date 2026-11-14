-- Library functions written in OrbitScript (ipairs, pairs, table.sort)
-- look like C functions: errors, tracebacks, debug.getinfo, yields.

local function err(f, ...) print(select(2, pcall(f, ...))) end
err(ipairs)
err(pairs)
err(table.sort, 1)
err(table.sort, {1, 2}, 3)
err(table.sort, {1, "x"})
print(select(2, pcall(function() table.sort(nil) end)))
print(select(2, pcall(function() local t = setmetatable({}, {__len = function() return 1.5 end}) table.sort(t) end)))

for _, f in ipairs({ipairs, pairs, table.sort}) do
  local i = debug.getinfo(f)
  print(i.what, i.short_src, i.source, i.linedefined, i.currentline, i.nups, i.nparams, i.isvararg)
  print(debug.getupvalue(f, 1))
  print(pcall(string.dump, f))
end
print(debug.getinfo(1, "l").currentline)

-- Inside a comparator, level 2 is sort, shown as C.
local shown = false
table.sort({1, 2}, function(a, b)
  if shown then return a < b end
  shown = true
  local i = debug.getinfo(2, "nSl")
  print(i.what, i.name, i.namewhat, i.currentline)
  print((debug.traceback("tb", 1):gsub("\n[^\n]*%(%.%.%.tail calls%.%.%.%)", "")))
  return a < b
end)

-- Sorting numbers and strings, with and without a comparator. (Which of
-- two equal elements comes first, and when a bad comparator is caught,
-- differ from lua5.5's.)
local t = {3, 1.5, 2, 1, -0.5, math.huge, -math.huge, 2^53}
table.sort(t) print(table.concat(t, " "))
t = {"b", "a", "c", "ab", ""} table.sort(t) print(table.concat(t, ","))
t = {5, 3, 4} table.sort(t, function(a, b) return a > b end) print(table.concat(t, " "))
t = setmetatable({}, {__index = function(_, k) return ({30, 10, 20})[k] end,
  __newindex = function(_, k, v) rawset(_, k, v) end, __len = function() return 3 end})
table.sort(t) print(rawget(t, 1), rawget(t, 2), rawget(t, 3))

-- Yields: refused across sort and the ipairs iterator, allowed across pairs.
local co = coroutine.wrap(function()
  print(pcall(table.sort, {1, 2, 3}, function(a, b) coroutine.yield() return a < b end))
  for _ in ipairs(setmetatable({}, {__index = function() print(coroutine.isyieldable()) end})) do end
  local f = pairs(setmetatable({}, {__pairs = function(t) coroutine.yield("in __pairs") return next, t end}))
  return "done"
end)
print(co()) print(co())

-- __pairs gets exactly four results through.
print(select("#", pairs(setmetatable({}, {__pairs = function() return 1, 2, 3, 4, 5 end}))))
print(select("#", pairs({})))

-- Negative indexes at the edge of the integer range are out of range, not a crash.
err(select, math.mininteger)
err(select, math.mininteger, 1, 2)
err(select, -3, 1, 2)
print(select(-2, "a", "b"))

-- Ranges at the edges of the integer range: errors or a count, not a crash.
local mi, ma = math.mininteger, math.maxinteger
err(table.move, {}, mi, ma, 1)
err(table.move, {}, mi + 1, ma, 1)
err(table.move, {}, 0, ma, 1)
err(table.move, {1, 2}, 1, 2, ma)
print(#table.move({1, 2}, 1, 2, ma - 1, {}))
print(table.move({1, 2}, ma, mi, 1)[1])
err(table.unpack, {}, mi, ma)
err(table.unpack, {}, 0, ma)
err(table.unpack, {}, mi, -1)
print(select("#", table.unpack({}, ma, ma)))
print(select("#", table.unpack({}, mi, mi + 2)))
-- Loops counting down from the top or up from the bottom with extreme steps.
for i = ma, mi, mi do io.write(i, " ") end print()
for i = mi, ma, ma do io.write(i, " ") end print()
for i = ma, ma - 2, -1 do io.write(i, " ") end print()
for i = mi, mi + 2 do io.write(i, " ") end print()
for i = 0, mi, mi + 1 do io.write(i, " ") end print()
