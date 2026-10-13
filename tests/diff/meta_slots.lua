-- Metatables whose metamethods are present, then replaced, removed (set to
-- nil), moved by rebuilds and shadowed by other keys reusing their slots.
-- Orbit remembers where a metatable's present metamethods live; the hash
-- part stays the source of truth, so every change must show at once.

local function show(...) print(...) end

local function ops(o, p)
  local r = {}
  local function try(name, f)
    local ok, v = pcall(f)
    r[#r + 1] = name .. "=" .. (ok and tostring(v) or "err")
  end
  try("add", function() return o + p end)
  try("sub", function() return o - 1 end)
  try("mul", function() return 2 * o end)
  try("unm", function() return -o end)
  try("eq", function() return o == p end)
  try("lt", function() return o < p end)
  try("le", function() return o <= p end)
  try("len", function() return #o end)
  try("concat", function() return o .. "s" end)
  try("call", function() return o(5) end)
  try("index", function() return o.missing end)
  try("band", function() return o & 1 end)
  try("idiv", function() return o // 2 end)
  return table.concat(r, " ")
end

local function sorted_keys(t)
  local keys = {}
  for k in pairs(t) do keys[#keys + 1] = k end
  table.sort(keys)
  return keys
end

local function full(tag)
  return {
    __add = function() return tag .. "+" end,
    __sub = function() return tag .. "-" end,
    __mul = function() return tag .. "*" end,
    __unm = function() return tag .. "u" end,
    __eq = function() return true end,
    __lt = function() return true end,
    __le = function() return false end,
    __len = function() return 42 end,
    __concat = function() return tag .. ".." end,
    __call = function(_, x) return tag .. "(" .. x .. ")" end,
    __index = function(_, k) return tag .. "." .. k end,
    __band = function() return tag .. "&" end,
    __idiv = function() return tag .. "//" end,
  }
end

-- Present, then replaced one by one, then removed one by one.
do
  local mt = full("A")
  local o, p = setmetatable({}, mt), setmetatable({}, mt)
  for _ = 1, 3 do show("full", ops(o, p)) end
  local keys = sorted_keys(full("x"))
  for _, k in ipairs(keys) do
    mt[k] = full("B")[k]
    show("replaced " .. k, ops(o, p))
  end
  for _, k in ipairs(keys) do
    mt[k] = nil
    show("removed " .. k, ops(o, p))
  end
  for _, k in ipairs(keys) do
    mt[k] = full("C")[k]
    show("restored " .. k, ops(o, p))
  end
end

-- Slots moved by rebuilds: the metatable grows (and shrinks back through
-- removals and regrowth) while its metamethods stay.
do
  local mt = full("R")
  local o, p = setmetatable({}, mt), setmetatable({}, mt)
  show("before grow", ops(o, p))
  for i = 1, 300 do
    mt["f" .. i] = i
    if i % 37 == 0 then show("grow " .. i, ops(o, p)) end
  end
  show("big", ops(o, p))
  for i = 1, 300 do mt["f" .. i] = nil end
  show("emptied extras", ops(o, p))
  for i = 1, 20 do rawset(mt, "g" .. i, i) end
  show("regrown", ops(o, p))
  mt.__add, mt.__index = nil, nil
  for i = 21, 60 do rawset(mt, "g" .. i, i) end
  show("removed then rebuilt", ops(o, p))
  mt.__add = function() return "R2+" end
  show("re-added", ops(o, p))
end

-- Removed metamethods whose dead slots other keys then take.
do
  local mt = {
    __index = function() return "i" end,
    __len = function() return 1 end,
    __eq = function() return true end,
  }
  local o, p = setmetatable({}, mt), setmetatable({}, mt)
  show("small", o.z, #o, o == p)
  mt.__index, mt.__len, mt.__eq = nil, nil, nil
  for i = 1, 3 do mt["x" .. i] = i end
  show("taken", o.z, #o, o == p)
  mt.__len = function() return 2 end
  mt.__index = { z = "tbl" }
  show("back", o.z, #o, o == p)
end

-- Tiny metatables, where several metamethods share little room.
do
  for n = 1, 6 do
    local mt = {}
    local o, p = setmetatable({}, mt), setmetatable({}, mt)
    local all = full("T" .. n)
    local keys = sorted_keys(all)
    for i = 1, n do mt[keys[(i * 5) % #keys + 1]] = all[keys[(i * 5) % #keys + 1]] end
    for _ = 1, 2 do show("tiny " .. n, ops(o, p)) end
  end
end

-- Cleared with next() while traversing, then filled again.
do
  local mt = full("N")
  local o, p = setmetatable({}, mt), setmetatable({}, mt)
  show("next before", ops(o, p))
  local n = 0
  for k in next, mt do
    mt[k] = nil
    n = n + 1
  end
  show("next cleared " .. n, ops(o, p))
  local m = full("M")
  for _, k in ipairs(sorted_keys(m)) do
    rawset(mt, k, m[k])
    show("next refilling " .. k, ops(o, p))
  end
  show("next refilled", ops(o, p))
end

-- A class hierarchy: __index chains through tables, methods change.
do
  local Base = {}
  Base.__index = Base
  function Base.name() return "base" end
  local Derived = setmetatable({}, Base)
  Derived.__index = Derived
  Derived.__tostring = function(s) return "D:" .. tostring(s.name and s.name()) end
  local obj = setmetatable({}, Derived)
  show("class", obj.name(), tostring(obj))
  function Derived.name() return "derived" end
  show("override", obj.name(), tostring(obj))
  Derived.name = nil
  show("unoverride", obj.name(), tostring(obj))
  Derived.__index = nil
  show("no index", obj.name, tostring(obj))
  Derived.__index = Base
  for i = 1, 50 do Derived["m" .. i] = i end
  show("reindexed", obj.name(), tostring(obj), obj.m1)
end

-- A weak-keyed metatable that is also used as one: collected keys leave
-- tombstones in its hash part.
do
  local mt = setmetatable({}, { __mode = "k" })
  for i = 1, 20 do mt[{}] = i end
  mt.__index = function(_, k) return "w" .. k end
  mt.__len = function() return 3 end
  local o = setmetatable({}, mt)
  show("weak", o.a, #o)
  collectgarbage()
  collectgarbage()
  show("weak collected", o.a, #o)
  mt.__index = nil
  show("weak removed", o.a, #o)
  for i = 1, 20 do mt[{}] = i end
  mt.__index = function() return "again" end
  collectgarbage()
  show("weak again", o.a, #o)
end

-- __newindex, __close, __call on shared metatables.
do
  local log = {}
  local mt = { __newindex = function(t, k, v) log[#log + 1] = k; rawset(t, k, v) end }
  local o = setmetatable({}, mt)
  o.a = 1
  mt.__newindex = function(t, k, v) log[#log + 1] = "2" .. k; rawset(t, k, v * 2) end
  o.b = 1
  mt.__newindex = nil
  o.c = 1
  show("newindex", table.concat(log, ","), o.a, o.b, o.c)
  mt.__close = function() log[#log + 1] = "closed" end
  do local x <close> = o end
  show("close", table.concat(log, ","))
end
