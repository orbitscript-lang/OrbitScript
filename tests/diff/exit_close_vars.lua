-- os.exit(code, true) closes the main thread's open to-be-closed
-- variables first, newest first and with a nil error; an error one raises
-- goes to the next. A coroutine's variables are not closed. Finalizers run
-- after the variables.
local keep = setmetatable({}, {__gc = function () print("gc") end})
local a <close> = setmetatable({}, {__close = function (_, e) print("a", e) end})
local b <close> = setmetatable({}, {__close = function () error("bad b", 0) end})
local function f()
  local c <close> = setmetatable({}, {__close = function (_, e)
    print("c", e)
    setmetatable({}, {__gc = function () print("gc from __close") end})
  end})
  coroutine.wrap(function ()
    local d <close> = setmetatable({}, {__close = function () print("never") end})
    os.exit(true, true)
  end)()
end
f()
