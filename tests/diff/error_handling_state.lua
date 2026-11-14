-- What an error carries while it unwinds: whether its message handler has
-- run, and whether it became an error in error handling.
local function tbc(f) return setmetatable({}, {__close = f}) end
local calls
local function counting(m) calls = calls + 1; return "handled(" .. tostring(m) .. ")" end

-- The handler runs once per error, before any variable closes, and not
-- again when a `__close` handler has run.
do
  calls = 0
  print(xpcall(function()
    local a <close> = tbc(function(_, e) print("close a", e, calls) end)
    local b <close> = tbc(function(_, e) print("close b", e, calls) end)
    error("e1", 0)
  end, counting))
  print(calls)
end

-- An error raised by a `__close` handler replaces the one unwinding, and
-- goes through the message handler itself.
do
  calls = 0
  print(xpcall(function()
    local a <close> = tbc(function(_, e) print("close a", e, calls) end)
    local b <close> = tbc(function(_, e) print("close b", e, calls); error("e2", 0) end)
    error("e1", 0)
  end, counting))
  print(calls)
  -- Caught inside the handler, it changes nothing.
  calls = 0
  print(xpcall(function()
    local a <close> = tbc(function(_, e) print("close a", e, pcall(error, "inner")) end)
    error("e1", 0)
  end, counting))
  print(calls)
  -- A nested protected call inside the handler has its own errors and its
  -- own message handler.
  calls = 0
  print(xpcall(function()
    local a <close> = tbc(function(_, e)
      print("close a", e, xpcall(function()
        local c <close> = tbc(function(_, e2) print("close c", e2, calls) end)
        error("inner", 0)
      end, counting))
    end)
    error("outer", 0)
  end, counting))
  print(calls)
end

-- Without a message handler: pcall, with the variables in several frames.
do
  local function level(n)
    local v <close> = tbc(function(_, e) print("close", n, e) end)
    if n == 0 then error("deep", 0) end
    level(n - 1)
  end
  print(pcall(level, 3))
  print(pcall(function()
    local v <close> = tbc(function(_, e) error(e .. "+", 0) end)
    local w <close> = tbc(function(_, e) error(e .. "+", 0) end)
    error("e", 0)
  end))
end

-- An error inside the message handler calls the handler again; one that
-- keeps failing ends as an error in error handling.
do
  local n = 0
  print(xpcall(error, function(m) n = n + 1; if n < 3 then error("again " .. n, 0) end return m .. " after " .. n end, "first"))
  n = 0
  print(xpcall(error, function(m) n = n + 1; error(m, 0) end, "forever"))
  print(n > 1)
  print(xpcall(error, function(m) local t; return t.x end, "handler fails"))
  -- The same from inside a coroutine, and a handler that is not callable.
  print(coroutine.wrap(function() return xpcall(error, function(m) error(m, 0) end, "in co") end)())
  print(pcall(xpcall, error, 42, "not callable"))
  print(pcall(xpcall, error))
end

-- A handler that returns something other than a message, or nothing.
do
  print(xpcall(error, function() end, "dropped"))
  print(xpcall(error, function(m) return m, "extra" end, "one result"))
  print(xpcall(error, function(m) return nil end))
  print(select("#", xpcall(error, function(m) return {} end, "table")))
  print(pcall(error))
  print(pcall(error, nil))
end

-- Stack overflow: an ordinary error outside a handler; inside a handler it
-- has some room, and past that is an error in error handling.
do
  local function over() return 1 + over() end
  local ok, msg = pcall(over)
  print(ok, string.find(msg, "stack overflow", 1, true) ~= nil)
  local ok, msg = xpcall(over, function(m) return "seen: " .. tostring(string.find(m, "stack overflow", 1, true) ~= nil) end)
  print(ok, msg)
  local ok, msg = xpcall(over, function(m) return over() end)
  print(ok, msg)
  -- Afterwards errors are ordinary again.
  print(pcall(error, "plain"))
  print(xpcall(error, counting, "plain"))
  -- The overflow inside a handler, caught by a pcall in that handler.
  local ok, msg = xpcall(error, function(m) return m .. ":" .. tostring((pcall(over))) end, "outer")
  print(ok, msg)
  print(xpcall(error, counting, "plain again"))
end

-- A handler whose error state must not leak into the next error.
do
  print(xpcall(function()
    print(xpcall(error, function(m) error(m, 0) end, "inner forever"))
    error("outer", 0)
  end, counting))
  print(xpcall(function()
    print(xpcall(error, counting, "inner"))
    local v <close> = tbc(function(_, e) print("close", e) end)
    error("outer", 0)
  end, counting))
end

-- Across coroutines: the handler belongs to the stack that set it.
do
  print(xpcall(function()
    local co = coroutine.wrap(function()
      local v <close> = tbc(function(_, e) print("co close", e) end)
      error("in co", 0)
    end)
    co()
  end, counting))
  print(xpcall(coroutine.wrap(function()
    print(xpcall(error, counting, "inside"))
    coroutine.yield("y")
    error("after yield", 0)
  end), counting))
  local co = coroutine.create(function()
    return xpcall(function()
      local v <close> = tbc(function(_, e) print("yielding close", e); coroutine.yield("from close") end)
      error("unwinding", 0)
    end, counting)
  end)
  print(coroutine.resume(co))
  print(coroutine.resume(co))
  print(coroutine.status(co))
end

-- `error` with a level, and errors raised by the handler machinery itself,
-- keep their positions.
do
  local function f() error("lvl2", 2) end
  local function g()
    f()
  end
  print(pcall(g))
  print(xpcall(g, function(m) return m .. "!" end))
  print(xpcall(g, debug.traceback) == false)
end
