-- Active lines and line hooks start at the first line of the body, even
-- when the prologue captures parameters or collects varargs.
local function show(f)
  local t = {}
  for k in pairs(debug.getinfo(f, "L").activelines) do t[#t + 1] = k end
  table.sort(t)
  print(table.concat(t, " "))
end
local function f1(...)
  return ...
end
local function f2(a, b)
  local g = function() return a end
  return g
end
local function f3(a) local g = function() return a end
  return g
end
local function f4(...) local t = {...}
  return t
end
show(f1) show(f2) show(f3) show(f4)
local lines = {}
debug.sethook(function(e, l) lines[#lines + 1] = l end, "l")
f1(1) f2(1) f3(1) f4(1)
debug.sethook()
print(table.concat(lines, " "))
