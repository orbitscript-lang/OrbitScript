-- Metatables and metamethods.
local V = {}
V.__index = V
local function vec(x, y) return setmetatable({x = x, y = y}, V) end
V.__add = function (a, b) return vec(a.x + b.x, a.y + b.y) end
V.__eq = function (a, b) return a.x == b.x and a.y == b.y end
V.__lt = function (a, b) return a.x < b.x end
V.__le = function (a, b) return a.x <= b.x end
V.__tostring = function (v) return "(" .. v.x .. ", " .. v.y .. ")" end
V.__len = function () return 2 end
V.__call = function (self, k) return self[k] end
V.__concat = function (a, b) return tostring(a) .. "|" .. tostring(b) end
V.__unm = function (a) return vec(-a.x, -a.y) end
V.__close = function () print("closed") end
function V:norm1() return math.abs(self.x) + math.abs(self.y) end
local a, b = vec(1, 2), vec(3, -4)
print(tostring(a + b), a == vec(1, 2), a < b, a <= b, #a, a("y"), a .. b, tostring(-b), b:norm1())
local log = {}
local proxy = setmetatable({}, {
  __index = function (_, k) log[#log + 1] = "get " .. k; return k:upper() end,
  __newindex = function (t, k, v) log[#log + 1] = "set " .. k; rawset(t, k, v * 2) end,
})
print(proxy.foo, proxy.bar); proxy.z = 21; print(proxy.z, table.concat(log, ";"))
print(getmetatable("abc").__index == string, getmetatable(setmetatable({}, {__metatable = "locked"})))
print(pcall(setmetatable, setmetatable({}, {__metatable = 1}), {}))
do local x <close> = a end
local mt = {__index = function (t, k) return k * 2 end}
print(setmetatable({}, mt)[21], rawget(setmetatable({}, mt), 21))
