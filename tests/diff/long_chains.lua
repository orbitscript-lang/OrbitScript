-- Chains of operators and suffixes as long as a program can make them compile and run:
-- each one is a tree as deep as it is long, so every pass has to walk it with a loop.
local N = 200000
-- lua5.5 takes time quadratic in the length of `and`, `or` and `elseif` chains.
local M = 5000

local function run(name, src, ...)
  local f, err = load(src, "=" .. name)
  if not f then
    print(name, "load error", err)
    return
  end
  print(name, pcall(f, ...))
end

local t = {v = 7}
t.b = t
t[1] = t
function t:m() self.v = self.v + 1; return self end
local function f() return f end
local calls = 0
local function g() calls = calls + 1; return g end

run("add", "return 1" .. string.rep(" + 1", N))
run("sub", "return 0" .. string.rep(" - 2", N))
run("bor", "return 0" .. string.rep(" | 1", N))
run("dot", "local t = ... return t" .. string.rep(".b", N) .. ".v", t)
run("index", "local t = ... return t" .. string.rep("[1]", N) .. ".v", t)
run("call", "local f = ... return f" .. string.rep("()", N) .. " == f", f)
run("calls counted", "local g = ... g" .. string.rep("()", N), g)
print("calls", calls)
run("method", "local t = ... return t" .. string.rep(":m()", N) .. ".v", t)
run("string args", "local f = ... return f" .. string.rep('"x"', N) .. " == f", f)
run("table args", "local f = ... return f" .. string.rep("{}", N) .. " == f", f)
run("and", "local a = ... return a" .. string.rep(" and a", M), 3)
run("and false", "local a = ... return a" .. string.rep(" and a", M) .. " and false", 3)
run("or", "return nil" .. string.rep(" or nil", M) .. " or 5")
run("or first", "return 4" .. string.rep(" or nil", M))
run("comparison", "local a = ... return a" .. string.rep(" < a", 3), 1)
run("eq", "return 1" .. string.rep(" == 1", N))
run("mixed", "local t = ... return t" .. string.rep(".b[1]:m()", N // 3) .. ".v + 1" ..
  string.rep(" * 1 + 1", N // 3), t)
run("elseif", "local a = ... if a == 0 then return 0 " ..
  string.rep("elseif a == 1 then return 1 ", M) .. "else return 2 end", 2)
run("condition", "local a = ... if a" .. string.rep(" and a", M) .. " then return 1 end", 1)
run("loop condition", "local a, n = ... while a" .. string.rep(" and a", M) ..
  " do n = n + 1 if n == 3 then a = false end end return n", true, 0)
