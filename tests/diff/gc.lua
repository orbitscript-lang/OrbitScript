-- Garbage collector interface: results must not depend on heap layout.
print(collectgarbage("isrunning"), type(collectgarbage("count")))
local weak = setmetatable({}, {__mode = "k"})
do weak[{}] = 1 end
collectgarbage(); collectgarbage()
print(next(weak))
local finalized = {}
do
  local _ = setmetatable({}, {__gc = function () finalized[#finalized + 1] = "a" end})
end
collectgarbage(); collectgarbage()
print(#finalized, finalized[1])
print(type(collectgarbage("step")), collectgarbage("incremental"), collectgarbage("incremental"))
