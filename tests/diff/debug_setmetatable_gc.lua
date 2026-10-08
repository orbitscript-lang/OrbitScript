-- debug.setmetatable with __gc registers the object for finalization,
-- as setmetatable does.
local log = {}
do
  local t = {}
  debug.setmetatable(t, {__gc = function() log[#log + 1] = "table" end})
end
collectgarbage()
collectgarbage()
print(#log, log[1])
