-- Library functions on values with metatables: __index, __newindex, __len,
-- __lt, __tostring and __eq through table, math and string functions, and
-- the string metatable's arithmetic metamethods.
local function P(...) local r = table.pack(pcall(...)) for i = 1, r.n do r[i] = tostring(r[i]) end print(table.concat(r, " ", 1, r.n)) end
local log = {}
local function proxy(n)
  local store = {}
  for i = 1, n do store[i] = i * 10 end
  return setmetatable({}, {
    __index = function(_, k) log[#log+1] = "g" .. tostring(k) return store[k] end,
    __newindex = function(_, k, v) log[#log+1] = "s" .. tostring(k) .. "=" .. tostring(v) store[k] = v end,
    __len = function() log[#log+1] = "len" return n end,
  }), store
end
local function dump(s, n) local o = {} for i = 1, n do o[i] = tostring(s[i]) end return table.concat(o, ",") end
local function flush() print(table.concat(log, " ")) log = {} end
local p, s = proxy(3)
table.insert(p, 99) flush() print(dump(s, 4))
p, s = proxy(3) table.insert(p, 2, 77) flush() print(dump(s, 4))
p, s = proxy(3) print(table.remove(p)) flush() print(dump(s, 3))
p, s = proxy(3) print(table.remove(p, 1)) flush() print(dump(s, 3))
p, s = proxy(0) print(table.remove(p)) flush()
p, s = proxy(3) print(table.concat(p, "-")) flush()
p, s = proxy(3) print(table.unpack(p)) flush()
p, s = proxy(3) print(table.unpack(p, 2, 3)) flush()
p, s = proxy(3) local d, ds = proxy(0) table.move(p, 1, 3, 2, d) flush() print(dump(ds, 4))
p, s = proxy(3) table.move(p, 1, 3, 2) flush() print(dump(s, 4))
p, s = proxy(4) s[1], s[2], s[3], s[4] = 3, 1, 4, 2 table.sort(p) log = {} print(dump(s, 4))
p, s = proxy(3) table.sort(p, function(a, b) return a > b end) log = {} print(dump(s, 3))
-- checktab errors
P(table.insert, 1, 2)
P(table.insert, setmetatable({}, {__index = {}}), 2)
P(table.insert, setmetatable({}, {__len = function() return 1 end}), 2)
P(table.concat, setmetatable({}, {__len = function() return 1 end}))
P(table.remove, "abc")
P(table.unpack, "abc")
local ud = setmetatable({}, {__len = function() return 2.0 end, __index = function(_, k) return k end})
P(table.unpack, ud)
P(table.unpack, setmetatable({}, {__len = function() return 2.5 end}))
P(table.unpack, setmetatable({}, {__len = function() return "2" end, __index = function(_, k) return k end}))
P(table.unpack, setmetatable({}, {__len = function() return "x" end}))
P(table.insert, setmetatable({}, {__len = function() return "x" end, __newindex = rawset}), 1)
P(table.concat, setmetatable({}, {__len = function() return 2 end, __index = function(_, k) return {} end}))
P(table.sort, setmetatable({}, {__len = function() return 2 end, __index = function(_, k) return {} end, __newindex = rawset}))
-- __lt
local mt = {__lt = function(a, b) return a.v < b.v end}
local t = {} for i, v in ipairs{5, 3, 8, 1} do t[i] = setmetatable({v = v}, mt) end
table.sort(t) local o = {} for i = 1, #t do o[i] = t[i].v end print(table.concat(o, ","))
print(math.max(t[1], t[2], t[3]).v, math.min(t[1], t[2], t[3]).v)
P(math.max, {}, {})
P(math.max, 1, "2")
P(table.sort, {1, "x"})
print(string.format("%s|%5s|%-6s|", setmetatable({}, {__tostring = function() return "TS" end}), setmetatable({}, {__tostring = function() return "ab" end}), setmetatable({}, {__name = "MyT"})):gsub("0x%x+", "ADDR"))
P(string.format, "%s", setmetatable({}, {__tostring = function() return 1 end}))
P(string.format, "%s", setmetatable({}, {__tostring = function() return {} end}))
print(string.format("%d %s", 3, setmetatable({}, {__name = 5})):gsub("0x%x+", "ADDR"))
print(("hello world"):gsub("%w+", setmetatable({}, {__index = function(_, k) return k:upper() end})))
print(("abc"):gsub("%w", setmetatable({}, {__index = function(_, k) if k == "b" then return false end return "<" .. k .. ">" end})))
do
local log = {}
local p = setmetatable({}, {__len = function() log[#log+1] = "len" return 2 end, __index = function(_, k) log[#log+1] = "g"..k return "x" end, __newindex = function(_, k) log[#log+1] = "s"..k end})
local function L(...) print(..., table.concat(log, " ")) log = {} end
L(pcall(table.concat, p, "-", 1, 2))
L(pcall(table.concat, p, {}))
L(pcall(table.concat, p, "-", {}))
L(pcall(table.insert, p))
L(pcall(table.insert, p, 1, 2, 3))
L(pcall(table.insert, p, 5, 2))
L(pcall(table.insert, p, "a", 2))
L(pcall(table.remove, p, 7))
L(pcall(table.remove, p, "a"))
L(pcall(table.sort, p, 3))
L(pcall(table.move, p, 1, 2, 1, 3))
L(pcall(table.move, 3, 1, 2, 1))
L(pcall(table.move, p, "a", 2, 1))
L(pcall(table.move, setmetatable({}, {__index = {}}), 1, 1, 1, setmetatable({}, {})) )
L(pcall(table.unpack, p, 1, 2))
print(pcall(table.insert, setmetatable({}, {__len = function() return 2^63 end}), 1))
print(pcall(table.insert, setmetatable({}, {__len = function() return math.maxinteger end, __newindex = function(t, k, v) print("set", k) end}), 1))
end
do
local log = {}
local p = setmetatable({}, {__len = function() log[#log+1] = "len" return 2 end, __index = function(_, k) log[#log+1] = "g"..k return "x" end})
local function L(...) print(..., table.concat(log, " ")) log = {} end
L(pcall(table.unpack, p, "a"))
L(pcall(table.unpack, p, 1, "b"))
L(pcall(table.unpack, p, 1, 1e10))
L(pcall(table.unpack, p, 3, 1))
local u = setmetatable({}, {__len = function() return 2 end})
print(pcall(table.unpack, 5))
print(pcall(table.concat, "ab"))
print(pcall(table.insert, nil, 1))
local s = getmetatable("")
print(pcall(table.concat, setmetatable({}, {}), ","))
end
do
print(pcall(function() return "10" + 1, "3" * "4", -"2", "7" // 2, "2" ^ 2, "7" % 3, "1" / 2 end))
print(pcall(function() return "abc" + 1 end))
print(pcall(function() return {} + "1" end))
local mt = getmetatable("")
local ks = {} for k in pairs(mt) do ks[#ks+1] = k end table.sort(ks) print(table.concat(ks, " "))
print(pcall(mt.__add, "1", "2"), pcall(mt.__add, "a", 1), pcall(mt.__add, {}, 1))
print(pcall(mt.__unm, "1"), pcall(mt.__unm, "x"))
print(pcall(function() return "0x10" + 0, " 5 " * 2, "1e1" + 0 end))
local mt = getmetatable("")
print(mt.__add("a", setmetatable({}, {__add = function(x, y) return "T", 2 end})))
print(pcall(mt.__idiv, "1", "0"), pcall(mt.__mod, "1", 0), mt.__idiv("7", "2.0"), mt.__pow("2", "10"), mt.__div(1, 2))
print(pcall(function() return getmetatable("").__add("a", 1) end))
print(pcall(function() return "1" + setmetatable({}, {__add = function(x, y) return "TT" end}) end))
print(pcall(function() return setmetatable({}, {__name = "N"}) + "a" end))
print(pcall(mt.__unm, "x", setmetatable({}, {__unm = function() return "U" end})))
end
