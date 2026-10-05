-- __index and __newindex: chains, functions, caches, integer keys, globals.
local base = {greet = function (self) return "hi " .. self.name end, kind = "base"}
base.__index = base
local mid = setmetatable({kind = "mid"}, base)
mid.__index = mid
local obj = setmetatable({name = "obj"}, mid)
print(obj:greet(), obj.kind, rawget(obj, "kind"), obj.missing)

-- A field cache must not keep answering after the key goes away.
local defaults = setmetatable({}, {__index = function (_, k) return "default " .. k end})
for i = 1, 3 do
  defaults.x = (i == 2) and "set" or nil
  print(i, defaults.x)
end

-- Integer keys: array holes and the hash part both reach __index.
local arr = setmetatable({1, nil, 3}, {__index = function (_, i) return -i end})
print(arr[1], arr[2], arr[3], arr[4], arr[100], arr[-1], arr[1.0], arr[2.5])
local key = 2
print(arr[key], arr[key + 2])

-- __newindex only runs for absent keys; rawset bypasses it.
local log = {}
local store = {}
local proxy = setmetatable({present = 1}, {
  __newindex = function (t, k, v) log[#log + 1] = tostring(k) .. "=" .. tostring(v); rawset(store, k, v) end,
  __index = store,
})
proxy.present = 2
proxy.a = 10
proxy[1] = 11
proxy[1] = 12
proxy[2.0] = 13
proxy.present = nil
proxy.present = 3
print(table.concat(log, " "), proxy.a, proxy[1], proxy[2], rawget(proxy, "a"), rawget(proxy, "present"))

-- __newindex as a table redirects the store.
local sink = {}
local redirect = setmetatable({}, {__newindex = sink})
redirect.k = "v"; redirect[7] = "seven"
print(rawget(redirect, "k"), sink.k, sink[7])

-- Indexing other types through their metatables.
print(("abc"):rep(2), ("hello"):sub(2, 3), ("x"):len(), #"four")
local s = "str"
print(s.len, pcall(function () return s.nope.deeper end))

-- Global access through a metatable on _ENV (strict mode).
setmetatable(_G, {
  __index = function (_, k) error("undeclared global '" .. k .. "'", 2) end,
  __newindex = function (t, k, v) rawset(t, k, v) end,
})
print(pcall(function () return undeclared_thing end))
declared = 5
print(declared)
setmetatable(_G, nil)

-- Errors and limits.
print(pcall(function () return setmetatable({}, {__index = 5}).x end))
local loop = {}; setmetatable(loop, {__index = loop})
print(pcall(function () return loop.x end))
local nlloop = {}; setmetatable(nlloop, {__newindex = nlloop})
print(pcall(function () nlloop.x = 1 end))
print(pcall(function () local n = nil; return n.x end))
print(pcall(function () local n = 5; n.x = 1 end))
print(pcall(function () return setmetatable({}, {__index = function () error("in index") end}).x end))

-- Deep recursion through __index is a catchable stack overflow.
local deep = setmetatable({}, {})
getmetatable(deep).__index = function (t, k) return t[k + 1] end
local ok, msg = pcall(function () return deep[1] end)
print(ok, msg:sub(-14) == "stack overflow" or msg)

-- Method calls on results of __index functions, and in loops.
local counter = setmetatable({}, {__index = function (t, k)
  local f = function (self, n) return k .. n end
  rawset(t, k, f)
  return f
end})
local acc = {}
for i = 1, 3 do acc[#acc + 1] = counter:step(i) end
print(table.concat(acc, ","))
