-- Metatables that gain a metamethod after a lookup found it missing.
-- Orbit remembers which metatable fields are absent; every way of adding
-- a key must forget that.

local function show(...) print(...) end

-- __index and __newindex, added by plain stores, after misses warmed up.
do
  local mt = {}
  local o = setmetatable({}, mt)
  for _ = 1, 3 do show("index before", o.x, o[1]) end
  mt.__index = function(_, k) return "idx:" .. tostring(k) end
  for _ = 1, 3 do show("index after", o.x, o[1]) end
  o.y = 1
  mt.__newindex = function(t, k, v) rawset(t, k, v * 10) end
  for i = 1, 3 do o["n" .. i] = i end
  show("newindex", o.n1, o.n2, o.n3, o.y)
  mt.__index = nil
  show("index removed", o.x)
  mt.__index = { x = "table" }
  show("index again", o.x)
end

-- Added with rawset, in a metatable large enough to rebuild while growing.
do
  local mt = {}
  for i = 1, 5 do mt["f" .. i] = i end
  local o = setmetatable({}, mt)
  show("raw before", o.k, #o)
  for i = 6, 40 do rawset(mt, "f" .. i, i) end
  rawset(mt, "__index", function() return "via rawset" end)
  rawset(mt, "__len", function() return 99 end)
  show("raw after", o.k, #o)
end

-- Cleared with next() while traversing, then filled again.
do
  local mt = { __index = function() return "first" end, a = 1, b = 2 }
  local o = setmetatable({}, mt)
  show("clear before", o.z)
  for k in next, mt do mt[k] = nil end
  show("clear mid", o.z, next(mt))
  show("clear mid2", o.z)
  mt.__index = function() return "second" end
  show("clear after", o.z)
end

-- __eq, __lt, __le, __concat, __unm, __call, __tostring, __name, __len.
do
  local mt = {}
  local a, b = setmetatable({}, mt), setmetatable({}, mt)
  show("eq before", a == b, pcall(function() return a < b end))
  show("call before", pcall(a))
  show("concat before", pcall(function() return a .. "x" end))
  show("unm before", pcall(function() return -a end))
  show("len before", #a)
  mt.__eq = function() return true end
  mt.__lt = function() return true end
  mt.__le = function() return false end
  mt.__concat = function() return "cat" end
  mt.__unm = function() return "neg" end
  mt.__call = function(_, x) return "called", x end
  mt.__len = function() return 7 end
  show("eq after", a == b, a < b, a <= b, a .. "x", -a, a(5), #a)
  show("tostring before", (tostring(a):gsub("0x%x+", "ADDR")))
  mt.__name = "Thing"
  show("name after", (tostring(a):gsub("0x%x+", "ADDR")))
  mt.__tostring = function() return "a thing" end
  show("tostring after", tostring(a))
end

-- __metatable added after getmetatable/setmetatable looked for it.
do
  local mt = {}
  local o = setmetatable({}, mt)
  show("protect before", getmetatable(o) == mt, pcall(setmetatable, o, mt))
  mt.__metatable = "locked"
  show("protect after", getmetatable(o), pcall(setmetatable, o, mt))
end

-- __pairs added later.
do
  local mt = {}
  local o = setmetatable({ 1 }, mt)
  local n = 0
  for _ in pairs(o) do n = n + 1 end
  mt.__pairs = function() return function(_, k) if not k then return 1, "p" end end, o, nil end
  local got = {}
  for k, v in pairs(o) do got[#got + 1] = tostring(k) .. "=" .. tostring(v) end
  show("pairs", n, table.concat(got, ","))
end

-- __close added after a failed attempt.
do
  local mt = {}
  local o = setmetatable({}, mt)
  show("close before", pcall(function() local x <close> = o end))
  mt.__close = function() print("closed") end
  do local x <close> = o end
end

-- __gc: only a metatable that has it when set registers the object.
do
  local mt = {}
  local o1 = setmetatable({}, mt)
  mt.__gc = function() print("gc o2") end
  local o2 = setmetatable({}, mt)
  o1, o2 = nil, nil
  collectgarbage()
  collectgarbage()
  local mt2 = {}
  setmetatable({}, mt2)
  mt2.__gc = function() print("gc o3") end
  local o3 = setmetatable({}, mt2)
  o3 = nil
  collectgarbage()
  collectgarbage()
end

-- __mode added after the collector has traced the table.
do
  local mt = {}
  local cache = setmetatable({}, mt)
  cache[1] = {}
  cache.k = {}
  collectgarbage()
  show("mode before", cache[1] ~= nil, cache.k ~= nil)
  mt.__mode = "v"
  collectgarbage()
  show("mode after", cache[1], cache.k)
end

-- A class hierarchy whose base gains methods after misses.
do
  local Base = {}
  Base.__index = Base
  local Derived = setmetatable({}, Base)
  Derived.__index = Derived
  local obj = setmetatable({}, Derived)
  for _ = 1, 3 do show("method before", obj.greet) end
  function Base.greet() return "hello" end
  for _ = 1, 3 do show("method after", obj.greet and obj:greet()) end
  Derived.greet = function() return "derived" end
  show("override", obj:greet())
end
