-- Finalizers, weak tables and collection during execution.
local order = {}
for i = 1, 3 do
  setmetatable({}, {__gc = function () order[#order + 1] = i end})
end
collectgarbage()
print(#order, order[1], order[2], order[3])
-- resurrection: a finalizer stores its object; it stays usable
local saved
do setmetatable({name = "phoenix"}, {__gc = function (o) saved = o end}) end
collectgarbage()
print(saved and saved.name)
saved = nil
collectgarbage()
-- weak values and keys
local wv = setmetatable({}, {__mode = "v"})
local wk = setmetatable({}, {__mode = "k"})
local keep = {}
wv[1] = {}; wv[2] = keep; wv[3] = "str"; wv[4] = 10
wk[{}] = 1; wk[keep] = 2
collectgarbage()
local nk = 0
for _ in pairs(wk) do nk = nk + 1 end
print(wv[1], wv[2] == keep, wv[3], wv[4], nk)
-- errors in finalizers become warnings (off by default) and do not propagate
setmetatable({}, {__gc = function () error("in gc") end})
collectgarbage()
print("survived")
-- lots of garbage while running keeps results intact
local acc = 0
for i = 1, 20000 do
  local t = {i, {i}}
  acc = acc + t[2][1]
end
print(acc)
local s = ""
for i = 1, 2000 do s = s .. "x" end
print(#s)
-- finalizers run at exit for live objects
_G.at_exit = setmetatable({}, {__gc = function () print("finalized at exit") end})
print(collectgarbage("isrunning"), collectgarbage("stop"), collectgarbage("isrunning"), collectgarbage("restart"))
