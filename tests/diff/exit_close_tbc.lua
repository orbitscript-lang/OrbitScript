-- os.exit(code, true) closes the main thread's variables still to close,
-- newest first, before the finalizers: the first handler gets no error,
-- later ones the latest error from a handler, and the last error is
-- dropped. A coroutine's variables are not closed.
local keep = setmetatable({}, {__gc = function() print("finalizer") end})
local function f2c(name, fail)
  return setmetatable({}, {__close = function(...)
    print(name, select("#", ...), select(2, ...))
    if fail then error(fail, 0) end
  end})
end
local a <close> = f2c("a")
local b <close> = f2c("b", "from b")
local co = coroutine.wrap(function()
  local c <close> = f2c("in coroutine")
  print(pcall(function()
    local d <close> = f2c("never")
    os.exit(true, true)
  end))
end)
print(pcall(function()
  local e <close> = f2c("e", "from e")
  local g <close> = f2c("g")
  co()
end))
