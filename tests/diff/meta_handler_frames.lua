-- Lua metamethod handlers run on their own frame: results, branches,
-- names in tracebacks and debug info, errors, yields and deep chains.
local mt = {}
mt.__index = function(t, k) return k .. "!" end
mt.__add = function(a, b) return 40 + b end
mt.__eq = function(a, b) return true end
mt.__lt = function(a, b) return false end
mt.__le = function(a, b) return nil end
mt.__newindex = function(t, k, v) rawset(t, k, v * 2) end
mt.__len = function() return 7 end
mt.__concat = function(a, b) return "cat" end
local p, q = setmetatable({}, mt), setmetatable({}, mt)
print(p.x, p[1], p + 2, p == q, p ~= q, p < q, p <= q, not (p < q), #p, p .. "s")
p.y = 21
print(rawget(p, "y"))
if p == q then print("eq branch") end
if p < q then print("never") else print("lt else") end

-- Names in debug info and tracebacks.
mt.__index = function(t, k)
  local info = debug.getinfo(1, "n")
  return tostring(info.namewhat) .. ":" .. tostring(info.name)
end
print(p.z)
mt.__index = function(t, k) error("boom " .. k) end
print(pcall(function() return p.w end))
mt.__index = function(t, k) return debug.traceback("tb", 1):match("in [^\n]*") end
print(p.v)

-- A tail call from a handler keeps finishing the instruction.
local function tail(k) return k .. "?" end
mt.__index = function(t, k) return tail(k) end
print(p.u)

-- Yielding inside a handler.
mt.__index = function(t, k) return coroutine.yield(k) end
local co = coroutine.wrap(function() local v = p.s; return "got " .. v end)
print(co())
print(co("resumed"))

-- A deep chain of handlers that call each other through metamethods.
local depth = 0
local deep = setmetatable({}, {__index = function(t, k)
  depth = depth + 1
  if k > 0 then return t[k - 1] end
  return depth
end})
print(deep[150])
print(pcall(function() return deep[100000] end))

-- The same with a hook armed.
local calls = 0
debug.sethook(function() calls = calls + 1 end, "c")
mt.__index = function(t, k) return k .. "#" end
local r = p.h
debug.sethook()
print(r, calls > 0)
