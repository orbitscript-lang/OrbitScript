-- _ENV as an ordinary name: shadowing, upvalues, assignment and error naming.
local print, pcall, setmeta = print, pcall, setmetatable
x = "global x"
print(_ENV.x, _ENV == _G)
do
  local _ENV = { y = "inner y" }
  print(y, x)
  z = 1
  print(_ENV.z)
end
print(z)
local function with_env(env, f) local _ENV = env return f() end
local function f()
  local _ENV <const> = { a = 10 }
  return function() return a end
end
print(f()())
local saved = _ENV
local function g() _ENV = { w = "swapped" } return w end
print(g())
print(saved.x)
_ENV = saved
print(x)
local ok, m = pcall(function() local _ENV = {} return nope.field end)
print(ok, m)
ok, m = pcall(function() local _ENV = {} nope() end)
print(ok, m)
ok, m = pcall(function() local _ENV = nil return q end)
print(ok, m)
ok, m = pcall(function() return _ENV.missing() end)
print(ok, m)
local function h(_ENV) return v end
print(h({ v = "param env" }))
