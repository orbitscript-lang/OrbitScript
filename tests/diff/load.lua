-- load, loadfile and dofile: chunk names, modes, environments, readers.

local function show(...)
  local t = {...}
  for i = 1, select("#", ...) do t[i] = tostring(t[i]) end
  print(select("#", ...), table.concat(t, " | "))
end

-- Source strings and chunk names in messages.
show(load("return 1 + 2")())
show(load("x = = 1"))
show(load("x = = 1", "=name"))
show(load("x = = 1", "@file.lua"))
show(load("\n\nx = = 1", "multi\nline"))
show(load("x = = 1", string.rep("long", 20)))
show(load("x = = 1", "@" .. string.rep("path/", 20)))
show(load("x = = 1", "=" .. string.rep("eq", 40)))
show(load(42))
show(load("return ...", "args")(1, nil, 3))

-- Modes.
show(load("return 1", "m", "b"))
show(load("return 1", "m", "t") ~= nil)
show(load("return 1", "m", "x"))
show(load("\27not really binary", "m", "t"))

-- Environments: a given env, even nil, replaces _ENV.
local env = {print = print}
load("y = 10; print(y)", "e", "t", env)()
print(env.y, y)
show(pcall(load("return x", "n", "t", nil)))
show(load("return x", "n", "t", {x = 123})())

-- Reader functions.
local parts = {"ret", "urn ", 4, "0 + ", "2"}
local i = 0
local f = load(function () i = i + 1; return parts[i] end)
show(f())
i = 0
show(load(function () i = i + 1; if i == 1 then return "return 1" else return "" end end)())
show(load(function () return nil end))
-- (lua5.5 appends a traceback to a reader's error: compare the first line.)
local function first_line(f, msg)
  local i = 1
  while i <= #msg and string.sub(msg, i, i) ~= "\n" do i = i + 1 end
  return f, string.sub(msg, 1, i - 1)
end
show(first_line(load(function () return {} end)))
show(first_line(load(function () error("reader failed") end)))
show(load(function () error({}) end) == nil)
show(pcall(load))
show(pcall(load, {}))

-- Files.
show(loadfile("does/not/exist.lua"))
show(pcall(dofile, "does/not/exist.lua"))

-- Mode B (fixed buffers) is for the C API only.
print(pcall(load, "", "", "B"))
print(pcall(load, function() return nil end, "", "B"))
print(pcall(load, "", "", "tx"))
