-- os.exit(code, true) closes the state first: every pending finalizer
-- runs, even when os.exit is called from one, and objects registered after
-- the close began are never finalized. Objects stay reachable so that no
-- collection finalizes them early.
local o1 = setmetatable({}, {__gc = function () print("gc 1") end})
local o2 = setmetatable({}, {__gc = function () error("in gc 2") end})
local o3 = setmetatable({}, {__gc = function ()
  print("gc 3")
  setmetatable({}, {__gc = function () print("never") end})
  print(collectgarbage() or false, collectgarbage("count") == nil)
  os.exit(0, true)
end})
local keep = setmetatable({}, {__gc = function () print("gc 4, still reachable") end})
io.write("buffered ")
print(collectgarbage("isrunning"))
os.exit(true, true)
