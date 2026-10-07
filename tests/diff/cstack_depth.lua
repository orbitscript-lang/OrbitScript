-- Calls back into the interpreter stop at lua5.5's depths ("C stack
-- overflow").
local n = 0
local function l2() n = n + 1; local ok, e = pcall(l2); if not ok then print(n, e) ; return end end
l2()
n = 0
local function l3() n = n + 1; local ok, e = pcall(string.gsub, "a", "a", l3); if not ok then print(n, e) ; return end end
l3()
n = 0
local co = coroutine.wrap(function() l2() end) co()
n = 0
local mt = {} mt.__index = function(t, k) n = n + 1; return setmetatable({}, mt)[k] end
print(pcall(function() return setmetatable({}, mt).x end), n)
n = 0
local mt = {} mt.__index = function(t, k) return setmetatable({}, mt)[k] end
print(pcall(function() return setmetatable({}, mt).x end))
local mt2 = {} mt2.__add = function(a, b) return setmetatable({}, mt2) + 1 end
print(pcall(function() return setmetatable({}, mt2) + 1 end))
local n = 0
local function l4() n = n + 1; return tostring(setmetatable({}, {__tostring = l4})) end
print(pcall(l4), n)
local function loop () assert(pcall(loop)) end
local err, msg = xpcall(loop, loop)
print((xpcall(loop, loop)))
