-- A `local function` is a visible local only once its closure exists: a line hook at its
-- definition must not see the register's previous value under the function's name.

local function locals()
  local out = {}
  local i = 1
  while true do
    local n, v = debug.getlocal(3, i)
    if not n then break end
    if n:sub(1, 1) ~= "(" then
      if type(v) == "function" or type(v) == "table" then v = type(v) end
      out[#out + 1] = n .. "=" .. tostring(v)
    end
    i = i + 1
  end
  return table.concat(out, " ")
end

local function test()
  do
    local tmp = "stale"
  end
  local function sum(a, b)
    return a + b
  end
  local function rec(n)
    if n == 0 then return 0 end
    return rec(n - 1)
  end
  return sum(1, 2) + rec(3)
end

local lines = {}
debug.sethook(function(_, line)
  if debug.getinfo(2, "f").func == test then
    lines[#lines + 1] = line .. ": " .. locals()
  end
end, "l")
local r = test()
debug.sethook()
for _, l in ipairs(lines) do print(l) end
print(r)
