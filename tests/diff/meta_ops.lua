-- Arithmetic, comparison, concatenation, length, call and conversion
-- metamethods.
local V = {}
V.__index = V
local function vec(x, y) return setmetatable({x = x, y = y}, V) end
local function show(v) return "(" .. v.x .. "," .. v.y .. ")" end
V.__add = function (a, b)
  if type(a) == "number" then return vec(a + b.x, a + b.y) end
  if type(b) == "number" then return vec(a.x + b, a.y + b) end
  return vec(a.x + b.x, a.y + b.y)
end
V.__sub = function (a, b) return vec(a.x - b.x, a.y - b.y) end
V.__mul = function (a, k) return vec(a.x * k, a.y * k) end
V.__div = function (a, k) return vec(a.x / k, a.y / k) end
V.__mod = function (a, k) return vec(a.x % k, a.y % k) end
V.__pow = function (a, k) return vec(a.x ^ k, a.y ^ k) end
V.__idiv = function (a, k) return vec(a.x // k, a.y // k) end
V.__unm = function (a, b) return vec(-a.x, -b.y) end
V.__eq = function (a, b) return a.x == b.x and a.y == b.y end
V.__lt = function (a, b) return a.x < b.x end
V.__le = function (a, b) return a.x <= b.x end
V.__len = function (a, b) return a.x + b.y end
V.__concat = function (a, b)
  local sa = type(a) == "table" and show(a) or tostring(a)
  local sb = type(b) == "table" and show(b) or tostring(b)
  return sa .. "&" .. sb
end
V.__call = function (self, ...) return select("#", ...), ... end
V.__tostring = function (v) return "vec" .. show(v) end

local a, b = vec(1, 2), vec(3, -4)
print(show(a + b), show(a + 10), show(10 + a), show(a - b), show(a * 3), show(b / 2))
print(show(b % 3), show(a ^ 2), show(b // 2), show(-a))
print(a == vec(1, 2), a ~= vec(1, 2), a == b, a < b, b < a, a <= b, a > b, a >= b)
print(#a, a(1, nil, 3), tostring(a), a .. b, "s" .. a, a .. 1 .. 2, 1 .. 2 .. a)

-- Comparison and equality rules.
local e = setmetatable({}, {__eq = function () return "truthy" end})
print(e == {}, {} == e, e == 1, e ~= {}, rawequal(e, e), rawequal(e, {}))
local lt = setmetatable({}, {__lt = function (x, y) return type(x) == "number" end})
print(lt < 1, 1 < lt, pcall(function () return lt <= 1 end))
print(pcall(function () return {} < {} end))
print(pcall(function () return 1 < "2" end))

-- Branches use metamethod results in conditions and loops.
local n = 0
local small = vec(0, 0)
while small < vec(5, 0) do small = small + vec(1, 0); n = n + 1 end
print(n, show(small))
if a == vec(1, 2) then print("eq branch") end
if not (a == b) then print("ne branch") end

-- Errors.
print(pcall(function () return setmetatable({}, {__add = 5}) + 1 end))
print(pcall(function () return {} + 1 end))
print(pcall(function () return 1 + {} end))
print(pcall(function () return "10" + {} end))
print(pcall(function () local t = {} return t .. "x" end))
print(pcall(function () return #setmetatable({}, {__len = function () error("len!") end}) end))
print(pcall(function () local c = setmetatable({}, {__call = 1}); c() end))
local selfcall = {}; setmetatable(selfcall, {__call = selfcall})
print(pcall(selfcall))
local chained = setmetatable({}, {__call = setmetatable({}, {__call = function (...) return select("#", ...) end})})
print(chained(1, 2))

-- __tostring, __name, __metatable.
print(tostring(setmetatable({}, {__name = "MyType"})):sub(1, 8))
print(pcall(tostring, setmetatable({}, {__tostring = function () return {} end})))
print(tostring(setmetatable({}, {__tostring = function () return 42 end})))
print(setmetatable({}, {__tostring = function () return "printed" end}), "plain")
local locked = setmetatable({}, {__metatable = "locked"})
print(getmetatable(locked), pcall(setmetatable, locked, {}))
print(getmetatable("abc").__index == string, getmetatable(1), getmetatable({}))
print(pcall(setmetatable, 1, {}))
print(pcall(setmetatable, {}, 1))
print(pcall(setmetatable, {}))

-- __pairs and ipairs through __index.
local p = setmetatable({}, {__pairs = function (t)
  return function (_, k) if not k then return 1, "one" end end, t, nil
end})
for k, v in pairs(p) do print("pairs", k, v) end
local ip = setmetatable({10}, {__index = function (_, i) if i < 4 then return i * 10 end end})
for i, v in ipairs(ip) do print("ipairs", i, v) end

-- A metamethod result feeding further arithmetic, under many iterations
-- (exercises the collector between metamethod calls).
local total = vec(0, 0)
for i = 1, 2000 do total = total + vec(i, -i) end
print(show(total))
