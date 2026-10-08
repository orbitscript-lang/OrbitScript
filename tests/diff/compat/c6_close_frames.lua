-- A `__close` handler run while an error unwinds sees the frames below the
-- protected call, not the frame that raised the error.
local function mk()
  return setmetatable({}, {__close = function()
    print(debug.traceback("tb", 1))
    local i = debug.getinfo(2, "Sn")
    print(i.what, i.name, i.namewhat)
  end})
end
local function f1() local y <close> = mk() error("boom") end
print(pcall(f1))
local function f2() local y <close> = mk() return 1 end
print(pcall(f2))
local function f3() do local y <close> = mk() end return 2 end
print(pcall(f3))
local co = coroutine.wrap(function() local y <close> = mk() coroutine.yield(1) error("x") end)
co() print(pcall(co))
