-- Closing the state finalizes every registered object, newest first;
-- objects registered while closing are not finalized.
local tt = {}
tt.__gc = function (o)
  print("fin", o.n)
  if o.n < 3 then setmetatable({n = o.n + 1}, tt) end
end
_G.keep = setmetatable({n = 10}, {__gc = function ()
  print("keep fin")
  setmetatable({n = 100}, tt)
end})
local u = setmetatable({n = 1}, tt)
_G.list = {}
for i = 1, 3 do _G.list[i] = setmetatable({n = "l" .. i}, {__gc = function (o) print("close", o.n) end}) end
do
  local resurrect = {}
  resurrect.__gc = function (o)
    _G.back = o
    setmetatable({}, resurrect)
    print("resurrected while closing")
  end
  _G.r1 = setmetatable({}, resurrect)
  _G.r2 = setmetatable({}, resurrect)
end
print("end of script")
