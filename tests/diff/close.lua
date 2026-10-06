-- To-be-closed variables (C3): scope exits, errors, coroutines.

local log = {}
local function say(...)
  local s = ""
  for i = 1, select("#", ...) do
    s = s .. (i > 1 and " " or "") .. tostring((select(i, ...)))
  end
  print(s)
end
local function mk(name)
  return setmetatable({}, { __close = function(o, e)
    say("close", name, e)
  end })
end

-- Falling out of blocks, newest first.
do
  local a <close> = mk("a")
  local b <close> = mk("b")
  local c <close> = nil
  local d <close> = false
  say("body")
end

-- break, goto, repeat, while, numeric for.
for i = 1, 3 do
  local x <close> = mk("for" .. i)
  if i == 2 then break end
end
local n = 0
while true do
  local w <close> = mk("while" .. n)
  n = n + 1
  if n == 2 then break end
end
n = 0
repeat
  local r <close> = mk("repeat" .. n)
  n = n + 1
until n == 2
do
  local i = 0
  ::top::
  do
    local z <close> = mk("goto" .. i)
    i = i + 1
    if i < 3 then goto top end
  end
end
do
  local j = 0
  while j < 2 do
    j = j + 1
    local q <close> = mk("continue" .. j)
    if j == 1 then goto continue end
    say("after goto", j)
    ::continue::
  end
end
do
  do
    local a <close> = mk("forward")
    goto out
  end
  local y = 1
  ::out::
end

-- Returns: values first, then closing, and no tail call.
local function many(...) return ... end
local function ret()
  local a <close> = mk("ret")
  return many(1, 2, 3)
end
say(ret())
local function ret_wide()
  local a <close> = mk("wide")
  return many(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20)
end
say(select("#", ret_wide()))
local function ret_local()
  local a <close> = mk("local")
  local v = "v"
  return v, a == nil
end
say(ret_local())

-- Generic for: the closing value.
local function iter(limit, name)
  return function(_, c) if c < limit then return c + 1 end end, nil, 0, mk(name)
end
for i in iter(3, "gen") do say("it", i) end
for i in iter(3, "genbreak") do if i == 2 then break end end
say(pcall(function() for i in iter(3, "generr") do error("in loop") end end))
say(pcall(function() for i in next, {}, nil, 42 do end end))

-- Errors: closes run while unwinding, each getting the error.
say(pcall(function()
  local a <close> = mk("e1")
  local b <close> = mk("e2")
  error("boom")
end))
say(pcall(function()
  local a <close> = setmetatable({}, { __close = function(_, e) error("C1:" .. tostring(e), 0) end })
  local b <close> = setmetatable({}, { __close = function(_, e) error("C2:" .. tostring(e), 0) end })
  error("E", 0)
end))
say(pcall(function()
  local a <close> = setmetatable({}, { __close = function() error("normal exit", 0) end })
end))
local function deep(k)
  local d <close> = mk("deep" .. k)
  if k == 0 then error("bottom", 0) end
  deep(k - 1)
end
say(pcall(deep, 3))
say(xpcall(function()
  local a <close> = mk("x")
  error("E", 0)
end, function(m) say("handler", m) return "H" end))
say(pcall(function() local x <close> = 42 end))
say(pcall(function() local x <close> = {} end))
say(pcall(function() local x <close> = setmetatable({}, { __close = true }) end))

-- A captured close variable.
do
  local c <close> = mk("captured")
  local f = function() return c ~= nil end
  say("captured", f())
end

-- Closing a value with __close through __index is not enough (raw).
say(pcall(function()
  local mt = { __index = { __close = function() end } }
  local x <close> = setmetatable({}, mt)
end))

-- Coroutines.
local co = coroutine.create(function()
  local a <close> = mk("co")
  coroutine.yield(1)
end)
coroutine.resume(co)
say(coroutine.close(co), coroutine.status(co))
co = coroutine.create(function()
  local a <close> = mk("co_err")
  error("X", 0)
end)
say(coroutine.resume(co))
say(coroutine.status(co))
say(coroutine.close(co))
say(coroutine.close(co))
co = coroutine.create(function()
  pcall(function()
    local a <close> = mk("in pcall")
    coroutine.yield()
  end)
  say("not reached")
end)
coroutine.resume(co)
say(coroutine.close(co))
co = coroutine.create(function()
  local a <close> = setmetatable({}, { __close = function(_, e) error("CE", 0) end })
  local b <close> = mk("before CE")
  coroutine.yield()
end)
coroutine.resume(co)
say(coroutine.close(co))
local w = coroutine.wrap(function()
  local a <close> = mk("wrap")
  error("W", 0)
end)
say(pcall(w))
co = coroutine.create(function()
  local a <close> = setmetatable({}, { __close = function()
    coroutine.yield("in close")
  end })
  return "done"
end)
say(coroutine.resume(co))
say(coroutine.resume(co))
say(coroutine.status(co))
co = coroutine.create(function()
  local a <close> = setmetatable({}, { __close = function(_, e)
    local v = coroutine.yield("unwinding " .. tostring(e))
    say("resumed in close with", v)
  end })
  error("U", 0)
end)
say(coroutine.resume(co))
co = coroutine.wrap(function()
  local ok, e = pcall(function()
    local a <close> = setmetatable({}, { __close = function(_, e)
      coroutine.yield("unwinding " .. tostring(e))
    end })
    error("U2", 0)
  end)
  return "caught", e
end)
say(co())
say(co())

-- Garbage while closing.
do
  local t <close> = setmetatable({}, { __close = function()
    for i = 1, 1000 do local _ = { i } end
    collectgarbage()
  end })
end

-- Closing nests like resuming: a long chain of closes overflows.
do
  local count, coro = 0, false
  for i = 1, 300 do
    local previous = coro
    coro = coroutine.create(function()
      local cc <close> = setmetatable({}, { __close = function()
        count = count + 1
        if previous then assert(coroutine.close(previous)) end
      end })
      coroutine.yield()
    end)
    assert(coroutine.resume(coro))
  end
  local st, msg = coroutine.close(coro)
  -- (lua5.5 adds a position per level and counts one more close.)
  say("chain", st, type(msg), count > 190)
end

-- A wrap whose resume is refused still raises that error.
do
  local depth = 0
  local function nest()
    depth = depth + 1
    return coroutine.wrap(nest)()
  end
  say("nested wraps", pcall(nest))
end
say("end")
