-- Errors raised right after a coroutine switch, in both directions, and what
-- the frames on either side report afterwards.
local function line(level) return debug.getinfo(level + 1, "l").currentline end

-- Into a coroutine whose body raises at once: a native body, a Lua body
-- whose first instruction fails, a body that isn't a function at all.
print(pcall(coroutine.wrap(error), "native body"))
print(pcall(coroutine.wrap(function() local t; return t.x end)))
print(select("#", pcall(coroutine.wrap(function(...) error({...}) end))))
print(coroutine.resume(coroutine.create(error), "resume native"))
print(pcall(coroutine.wrap(coroutine.wrap(error)), "nested"))
print(select("#", pcall(coroutine.wrap(string.rep))))

-- The frame that resumed keeps its own position: an error on a later line
-- of the same function reports that line.
do
  local t
  local function f()
    pcall(coroutine.wrap(error), "x")
    coroutine.resume(coroutine.create(error), "y")
    local here = line(1)
    local ok, msg = pcall(function()
      local w = coroutine.wrap(error)
      pcall(w, "z")

      return t.field
    end)
    return here, msg
  end
  print(f())
end

-- Back out of a coroutine: the error crosses the switch into the resumer,
-- which raises it at the call.
do
  local function thrower() error("from co") end
  local function resumer()
    local w = coroutine.wrap(thrower)
    w()
  end
  local ok, msg = pcall(resumer)
  print(ok, msg)
  local ok, msg = xpcall(resumer, function(m) return m .. " @" .. line(2) end)
  print(ok, msg)
end

-- After a yield, the resumer fails on its very next instruction; after the
-- resume, so does the coroutine.
do
  local t
  local co = coroutine.wrap(function()
    coroutine.yield()
    return t.after_resume
  end)
  print(pcall(function()
    co()
    return t.after_yield
  end))
  print(pcall(co))
end

-- An error thrown through several coroutines at once.
do
  local function deep(n)
    if n == 0 then error("bottom", 0) end
    return coroutine.wrap(deep)(n - 1)
  end
  print(pcall(deep, 5))
  print(pcall(coroutine.wrap(deep), 5))
end

-- The coroutine ends (or yields) with more values than its resumer's stack
-- takes: the error belongs to the resumer.
do
  local big = table.pack(table.unpack({}, 1, 200))
  local function many() return table.unpack(big, 1, 200) end
  print(select("#", coroutine.wrap(many)()))
  local function rec(n) if n == 0 then return coroutine.wrap(many)() end return (rec(n - 1)) end
  print(pcall(rec, 100))
end

-- A swallowed error that crossed a switch leaves nothing behind: `load`
-- reports its reader's error, a finalizer's error becomes a warning, and
-- the next error in the frame still has its line.
do
  local t
  local function boom() coroutine.wrap(error)("swallowed") end
  print(pcall(function()
    print((load(boom)))

    return t.after_load
  end))
  print(pcall(function()
    setmetatable({}, {__gc = boom})
    collectgarbage()

    return t.after_gc
  end))
  print(pcall(function()
    local s = string.gsub("abc", "b", function() pcall(boom) end)

    return t.after_gsub
  end))
  print(pcall(function()
    print(pcall(string.gsub, "abc", "b", boom))

    return t.after_gsub_error
  end))
  print(pcall(function()
    print(pcall(table.sort, {3, 2, 1}, function(a, b) boom() end))

    return t.after_sort
  end))
  print(pcall(function()
    local mt = {__index = function() boom() end, __tostring = function() boom() end}
    print(pcall(tostring, setmetatable({}, mt)))

    return t.after_tostring
  end))
end

-- Errors and switches inside metamethods.
do
  local t
  local mt = {}
  mt.__index = function(_, k) return coroutine.wrap(function() error("index " .. k, 0) end)() end
  mt.__add = function() coroutine.yield("in add"); error("add", 0) end
  mt.__lt = function() return coroutine.wrap(error)("lt") end
  local o = setmetatable({}, mt)
  print(pcall(function() return o.missing end))
  print(pcall(function() return o < o end))
  local co = coroutine.create(function() return o + 1 end)
  print(coroutine.resume(co))
  print(coroutine.resume(co))
  print(coroutine.status(co))
  print(pcall(function()
    local co = coroutine.wrap(function() return o + 1 end)
    co()
    pcall(co)

    return t.after_meta
  end))
end

-- Closing a coroutine whose handler raises: the closer gets the error and
-- keeps its position.
do
  local t
  local function tbc(f) return setmetatable({}, {__close = f}) end
  print(pcall(function()
    local co = coroutine.create(function()
      local x <close> = tbc(function() error("close failed", 0) end)
      coroutine.yield()
    end)
    coroutine.resume(co)
    print(coroutine.close(co))

    return t.after_close
  end))
  local co = coroutine.wrap(function()
    local x <close> = tbc(function() coroutine.yield("closing") error("late", 0) end)
    error("first", 0)
  end)
  print(pcall(co))
  print(pcall(co))
  print(pcall(co))
end

-- Resuming what can't be resumed raises before any switch.
do
  local co
  co = coroutine.wrap(function() return pcall(co) end)
  print(co())
  print(pcall(co))
  local running = coroutine.running()
  print(coroutine.resume(running))
  print(pcall(coroutine.yield))
end
