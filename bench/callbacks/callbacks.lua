-- What one call back into script code costs, route by route: script to
-- script, the interpreter to a metamethod, library functions to the
-- functions they are given, and the built-ins written in OrbitScript.
-- `orbit-testkit callbacks` runs each row at two sizes under callgrind and
-- takes the difference, so start-up and set-up cancel out (rows.txt).
--
-- usage: callbacks.lua ROW N
local row, n = arg[1], tonumber(arg[2])

local data = {}
for i = 1, 100 do data[i] = i end
local function id(x) return x end
local function idx(_, i) return data[i] end
local proxy = setmetatable({}, {__index = idx, __len = function() return 100 end})
local obj = setmetatable({}, {
  __tostring = function() return "o" end,
  __lt = function() return false end,
  __call = function(_, x) return x end,
  __add = function(a, b) return 1 end,
  __eq = function() return true end,
  __concat = function() return "" end,
})
local obj2 = setmetatable({}, getmetatable(obj))
local s = 0

local rows = {}

-- The loop the other rows run in.
function rows.empty() for i = 1, n do s = s + i end end

-- Script to script.
function rows.call() for i = 1, n do s = s + id(i) end end

-- The interpreter calls a metamethod.
function rows.meta_index() for i = 1, n do s = s + proxy[i % 100 + 1] end end
function rows.meta_add() for i = 1, n do s = s + (obj + i) end end
function rows.meta_call() for i = 1, n do s = s + obj(i) end end
function rows.meta_eq()
  local c = 0
  for _ = 1, n do if obj == obj2 then c = c + 1 end end
  s = c
end

-- Library functions calling back.
function rows.pcall() for i = 1, n do local _, v = pcall(id, i); s = s + v end end
function rows.xpcall() for i = 1, n do xpcall(id, id, i) end end
function rows.sort()
  local t = {}
  for i = 1, n / 16 do t[i] = (i * 7919) % 10007 end
  table.sort(t, function(a, b) return a < b end)
end
function rows.sort_plain()
  local t = {}
  for i = 1, n / 16 do t[i] = (i * 7919) % 10007 end
  table.sort(t)
end
function rows.gsub_fn()
  local str = ("x"):rep(100)
  local f = function() return "y" end
  for _ = 1, n / 100 do string.gsub(str, "x", f) end
end
function rows.gsub_str()
  local str = ("x"):rep(100)
  for _ = 1, n / 100 do string.gsub(str, "x", "y") end
end
function rows.tostring() for _ = 1, n do tostring(obj) end end
function rows.format_s() for _ = 1, n do string.format("%s", obj) end end
function rows.max_lt() for _ = 1, n do math.max(obj, obj2) end end
function rows.concat_proxy() for _ = 1, n / 100 do table.concat(proxy) end end
function rows.concat_plain() for _ = 1, n / 100 do table.concat(data) end end
function rows.unpack_proxy() for _ = 1, n / 100 do table.unpack(proxy, 1, 100) end end
function rows.unpack_plain() for _ = 1, n / 100 do table.unpack(data, 1, 100) end end
function rows.insert_proxy()
  local p = setmetatable({}, {
    __index = idx,
    __newindex = function() end,
    __len = function() return 5 end,
  })
  for i = 1, n do table.insert(p, i) end
end
function rows.insert_plain()
  local t = {}
  for i = 1, n do table.insert(t, i) end
end
function rows.ipairs()
  for _ = 1, n / 100 do for _, v in ipairs(data) do s = s + v end end
end
function rows.ipairs_proxy()
  for _ = 1, n / 100 do for i in ipairs(proxy) do if i == 100 then break end end end
end
function rows.pairs()
  for _ = 1, n / 100 do for _, v in pairs(data) do s = s + v end end
end
function rows.iter_closure()
  local function it(_, i) if i < 100 then return i + 1 end end
  for _ = 1, n / 100 do for i in it, nil, 0 do s = s + i end end
end
function rows.gmatch()
  local str = ("x"):rep(100)
  for _ = 1, n / 100 do for _ in string.gmatch(str, "x") do end end
end
function rows.load_reader()
  local parts = {}
  for i = 1, 100 do parts[i] = "local a = 1 " end
  for _ = 1, n / 100 do
    local i = 0
    load(function() i = i + 1; return parts[i] end)
  end
end
function rows.coroutine()
  local co = coroutine.wrap(function() while true do coroutine.yield(1) end end)
  for _ = 1, n do s = s + co() end
end
function rows.select() for i = 1, n do s = s + select("#", i, i) end end
function rows.native_leaf()
  local abs = math.abs
  for i = 1, n do s = s + abs(i) end
end

local f = rows[row]
if not f then error("no row " .. tostring(row)) end
f()
