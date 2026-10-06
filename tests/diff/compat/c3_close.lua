-- To-be-closed variables (front end part in C1, runtime in C3).
local function closer(name) return setmetatable({}, { __close = function(_, e) print("close", name, e) end }) end
do
  local a <close> = closer("a")
  local b <close> = closer("b")
  local c <close> = nil
  print("in block")
end
local function f()
  local x <close> = closer("x")
  return "returned"
end
print(f())
for i = 1, 2 do
  local y <close> = closer("y" .. i)
  if i == 2 then break end
end
print(pcall(function()
  local z <close> = closer("z")
  error("boom")
end))
print(load("local a <close>, b <close> = 1, 2"))
print(load("local a <close> = 1; a = 2"))
print(pcall(function() local q <close> = 42 end))
