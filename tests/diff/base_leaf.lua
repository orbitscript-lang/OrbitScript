-- The base library's leaf functions, through their fast paths (one result
-- kept, or exactly one given) and their full natives (more results kept,
-- errors, odd arguments).

local function show(...)
  local out = {select("#", ...)}
  for i = 1, select("#", ...) do
    out[#out + 1] = tostring((select(i, ...)))
  end
  print(table.concat(out, " "))
end

-- type
local locked = setmetatable({}, {__metatable = "locked"})
show(type(nil), type(true), type(1), type(1.5), type("s"), type({}))
show(type(print), type(show), type(coroutine.create(print)), type(io.stdout))
show(pcall(type))
local t1 = type(2)
show(t1, type(2) == "number")

-- getmetatable and setmetatable
show(getmetatable(locked), getmetatable({}), getmetatable("x") ~= nil)
local g = getmetatable(locked)
show(g)
show(pcall(setmetatable, locked, {}))
show(pcall(setmetatable, {}))
show(pcall(setmetatable, {}, 1))
show(pcall(setmetatable, 1, {}))
local mt = {}
show(setmetatable({}, mt) ~= nil, getmetatable(setmetatable({}, mt)) == mt)
show(getmetatable(setmetatable({}, nil)))

-- tonumber
show(tonumber("0x10"), tonumber(" 12 "), tonumber("z"), tonumber(5.5), tonumber({}))
show(tonumber("10", 2), tonumber("ff", 16), tonumber("8", 8), tonumber(" -zz ", 36))
show(pcall(tonumber))
show(pcall(tonumber, 1, 10))
show(pcall(tonumber, "1", 99))
show(tonumber("1e2"), tonumber("7", nil), tonumber(nil))
local n1 = tonumber("3")
show(n1, math.type(n1))

-- select
show(select("#"), select("#", nil, nil))
show(select(2, "a", "b", "c"))
show(select(-1, "a", "b"))
local s1 = select(2, "a", "b", "c")
local s2, s3 = select(-2, 1, 2, 3)
local s4 = select(5, 1, 2)
show(s1, s2, s3, s4, select("#", select(5, 1, 2)))
show(pcall(select, 0, 1))
show(pcall(select, -3, 1))
show(pcall(select, "x"))
show(pcall(select, math.mininteger, 1))
show(select(2.0, "a", "b"))

-- rawequal, rawget, rawset, rawlen
show(rawequal(locked, locked), rawequal(1, 1.0), rawequal("a", "a"), rawequal({}, {}))
show(pcall(rawequal, 1))
show(rawget({x = 1}, "x"), rawget({}, nil), rawget({[2] = 4}, 2.0))
show(pcall(rawget, 1, 2))
show(pcall(rawget, {}))
local proxy = setmetatable({}, {__index = function() return "meta" end})
show(rawget(proxy, "k"), proxy.k)
show(rawset({}, "a", 1).a)
show(pcall(rawset, {}, nil, 1))
show(pcall(rawset, {}, 0 / 0, 1))
show(pcall(rawset, {}, 1))
show(rawlen({1, 2}), rawlen("abc"), rawlen(setmetatable({1}, {__len = function() return 9 end})))
show(pcall(rawlen, 1))
show(pcall(rawlen))
local l = rawlen("four")
show(l)

-- assert
show(assert(1, 2, 3))
show(pcall(assert, false))
show(pcall(assert, nil, "m"))
show(pcall(assert, false, nil))
show(pcall(assert))
local a1 = assert("v", "unused")
show(a1, select("#", assert(1, 2, 3)), (assert("only")))

-- next
local t = {10, 20, x = 1}
local k = next(t)
show(k, next({}), select("#", next({})))
show(next(t, "x"))
show(next(t, 1))
local k2 = next(t, 1)
show(k2)
show(pcall(next, t, "nope"))
show(pcall(next, 1))
show(pcall(next))

-- warn checks every piece before writing any
show(pcall(warn))
show(pcall(warn, "a", {}))

-- Keeping two results skips the fast paths.
local a, b = type("s"); show(a, b)
a, b = getmetatable(locked); show(a, b)
a, b = getmetatable({}); show(a, b)
a, b = getmetatable(setmetatable({}, mt)); show(a == mt, b)
a, b = tonumber("0x1p4"); show(a, b)
a, b = tonumber(3); show(a, b)
a, b = tonumber({}); show(a, b)
a, b = tonumber("nope"); show(a, b)
a, b = select("#", 1, 2); show(a, b)
a, b = select(2, 1, 2, 3); show(a, b)
a, b = rawequal(1, 1); show(a, b)
a, b = rawget({5}, 1); show(a, b)
a, b = rawlen({1, 2, 3}); show(a, b)
a, b = rawlen("xy"); show(a, b)
a, b = assert(1); show(a, b)
a, b = next({}); show(a, b)
