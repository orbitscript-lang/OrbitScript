-- Compile errors at the limits: 200 local variables and 255 upvalues per function, with
-- the function and token each message names.
local function show(f, e) print(f and "ok" or e) end
local function locals(n, tail)
  local s = "\nfunction foo ()\n  local "
  for j = 1, n do s = s .. "a" .. j .. ", " end
  return s .. "b" .. (tail or "\n")
end
show(load(locals(199)))
show(load(locals(200)))
show(load(locals(200, " = 1 end")))
show(load(locals(199, "\nlocal c, d\nend")))
show(load("local " .. ("x,"):rep(250) .. "y"))
show(load("for i=1,2 do local " .. ("x,"):rep(197) .. "y end"))
show(load("local function f(" .. ("x,"):rep(199) .. "y) end"))
show(load("local function f(" .. ("x,"):rep(200) .. "y) end"))
local function th(pre, post)
  for n = 190, 202 do
    local ok, e = load(pre .. "local " .. ("x,"):rep(n - 1) .. "y" .. post)
    if not ok then return n, e end
  end
end
print("main", th("", ""))
print("numfor", th("for i = 1, 2 do ", " end"))
print("genfor", th("for k, v in pairs({}) do ", " end"))
print("genfor1", th("for k in pairs({}) do ", " end"))
print("func", th("local function f() ", " end"))
print("method", th("local t = {} function t:m() ", " end"))
print("vararg", th("local function f(...) ", " end"))
print("global", th("global x; ", ""))
print("lf", th("local function g() end ", ""))
print("close", th("local c <close> = nil ", ""))
print("const", th("local c <const> = 1 ", ""))
print("const2", th("local c <const> = {} ", ""))
print("nested", th("do local a end ", ""))
print("repeat", th("repeat ", " until true"))
print("vararg main fn", th("local function f(a) ", " end"))
print("vararg2", th("local function f(a, ...) ", " end"))
print("vararg named", th("local function f(...t) ", " end"))
print("conststr", th("local c <const> = 'x' ", ""))
print("constexpr", th("local c <const> = 1+2 ", ""))
print("constnil", th("local c <const> = nil ", ""))
print("consttrue", th("local c <const> = true ", ""))
print("constneg", th("local c <const> = -1 ", ""))
print("constlong", th("local c <const> = '" .. ("x"):rep(50) .. "' ", ""))
print("constcat", th("local c <const> = 'a'..'b' ", ""))
print("constf", th("local c <const> = 1.5 ", ""))
print("constdiv0", th("local c <const> = 1//0 ", ""))
print("constmulti", th("local a <const>, b = 1, 2 ", ""))
print("constmulti2", th("local a <const>, b <const> = 1 ", ""))
print("globalconst", th("local <const> a, b = 1, {} ", ""))
-- where: params
for n = 199, 201 do show(load("local function f(" .. ("x,"):rep(n - 1) .. "y\n)\n end")) end
show(load("for " .. ("x,"):rep(198) .. "y in next, {} do end"))
show(load("for " .. ("x,"):rep(198) .. "y\nin next, {} do end"))
show(load("local " .. ("x,"):rep(200) .. "y = 1\n\n;"))
show(load("local function f() local " .. ("x,"):rep(199) .. "y end local function g() end"))
show(load("local " .. ("x,"):rep(198) .. "y local function g() end"))
show(load("local " .. ("x,"):rep(199) .. "y\nlocal\nfunction\ng() end"))
local function ups(n, use, extra)
  local s = "local " for i = 1, n do s = s .. "u" .. i .. (i < n and "," or "") end
  s = s .. "\nreturn function()\n return function()\n local x = "
  for i = 1, n do s = s .. "u" .. i .. (i < n and "\n+" or "") end
  return s .. (extra or "") .. "\nend end"
end
show(load(ups(200)))
-- 255 via two levels of locals
local s = "local function a()\n local " for i=1,150 do s = s .. "p"..i.."," end s = s .. "q\n"
s = s .. "local function b()\n local " for i=1,150 do s = s .. "r"..i.."," end s = s .. "z\n"
s = s .. "return function()\n return p1"
for i=2,150 do s = s .. "+p"..i end
for i=1,150 do s = s .. "+r"..i end
show(load(s .. "\n end end end"))
-- with a global (needs _ENV)
s = "local function a()\n local " for i=1,150 do s = s .. "p"..i.."," end s = s .. "q\n"
s = s .. "local function b()\n local " for i=1,150 do s = s .. "r"..i.."," end s = s .. "z\n"
s = s .. "return function()\n return G + p1"
for i=2,150 do s = s .. "+p"..i end
for i=1,104 do s = s .. "+r"..i end
show(load(s .. "\n end end end"))
s = "local function a()\n local " for i=1,150 do s = s .. "p"..i.."," end s = s .. "q\n"
s = s .. "local function b()\n local " for i=1,150 do s = s .. "r"..i.."," end s = s .. "z\n"
s = s .. "return function()\n return p1"
for i=2,150 do s = s .. "+p"..i end
for i=1,104 do s = s .. "+r"..i end
show(load(s .. "\n end end end"))
show(load(s .. "+G\n end end end"))
-- deeper: middle function gets upvalues too
s = "local function a()\n local " for i=1,150 do s = s .. "p"..i.."," end s = s .. "q\n"
s = s .. "local function b()\n local " for i=1,150 do s = s .. "r"..i.."," end s = s .. "z\n"
s = s .. "return function()\n return function() return p1"
for i=2,150 do s = s .. "+p"..i end
for i=1,106 do s = s .. "+r"..i end
show(load(s .. "\n end end end end"))
-- assignment target as upvalue
s = "local function a()\n local " for i=1,150 do s = s .. "p"..i.."," end s = s .. "q\n"
s = s .. "local function b()\n local " for i=1,150 do s = s .. "r"..i.."," end s = s .. "z\n"
s = s .. "return function()\n local _ = p1"
for i=2,150 do s = s .. "+p"..i end
for i=1,105 do s = s .. "+r"..i end
show(load(s .. "\n r106 = 1 end end end"))
-- 199 locals then numeric for / generic for / local function / params
show(load("local " .. ("x,"):rep(197) .. "y\nfor i = 1, 2 do end"))
show(load("local " .. ("x,"):rep(197) .. "y\nfor i = 1, 2\ndo\nend"))
show(load("local " .. ("x,"):rep(196) .. "y\nfor k, v in next, {}\ndo\nend"))
show(load("local " .. ("x,"):rep(195) .. "y\nfor k, v in next, {}\ndo\nend"))
show(load("local " .. ("x,"):rep(198) .. "y\nfor i = 1, 2\ndo\nend"))
show(load("local " .. ("x,"):rep(199) .. "y\nfor i = 1, 2\ndo\nend"))
show(load("local function f(" .. ("x,"):rep(198) .. "...\n)\n end"))
show(load("local function f(" .. ("x,"):rep(199) .. "...\n)\n end"))
show(load("local t = {} function t:f(" .. ("x,"):rep(199) .. "y\n)\n end"))
