-- First-light subset: locals, upvalues, closures, recursion, tail calls, varargs.
local function counter()
  local n = 0
  return function() n = n + 1; return n end, function() return n end
end
local inc, get = counter()
inc(); inc()
print(get(), inc(), get())

-- each iteration gets a fresh binding
local fs = {}
for i = 1, 3 do fs[i] = function() return i end end
print(fs[1](), fs[2](), fs[3]())
local ws, j = {}, 0
while j < 3 do
  j = j + 1
  local k = j * 10
  ws[j] = function() k = k + 1; return k end
end
print(ws[1](), ws[1](), ws[2](), ws[3]())

-- captures through several levels, shared between siblings
local function outer()
  local x = 1
  local function mid()
    return function() x = x * 2; return x end
  end
  return mid(), function() return x end
end
local dbl, peek = outer()
dbl(); dbl()
print(peek())

-- shadowing
local s = "outer"
do local s = "inner"; print(s) end
print(s)

-- recursion and proper tail calls
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print(fib(20))
local function loop(n, acc) if n == 0 then return acc end return loop(n - 1, acc + 1) end
print(loop(200000, 0))
local function is_even(n) if n == 0 then return true end return is_odd(n - 1) end
function is_odd(n) if n == 0 then return false end return is_even(n - 1) end
print(is_even(10), pcall(is_even, 7))

-- multiple results and varargs
local function three() return 1, 2, 3 end
local a, b, c, d = three()
print(a, b, c, d)
print(three(), three())
print((three()))
local function va(...) return select("#", ...), ... end
print(va(), va(nil), va(1, nil, 3))
print(select(2, "a", "b", "c"), select(-1, "a", "b", "c"))
local function pack(...) return {...} end
print(#pack(three()), #pack(three(), 10))

-- global functions and assignment order
function greet(name) return "hello " .. name end
print(greet("orbit"))
local x, y = 1, 2
x, y = y, x
print(x, y)
