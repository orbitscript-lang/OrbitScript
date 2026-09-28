-- Closures, upvalues, recursion, varargs (including 5.5 named varargs).
local function counter()
  local n = 0
  return function () n = n + 1; return n end, function () return n end
end
local inc, get = counter()
inc(); inc()
print(get(), inc(), get())
local fs = {}
for i = 1, 3 do fs[i] = function () return i end end
print(fs[1](), fs[2](), fs[3]())
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print(fib(20))
local function va(...) return select("#", ...), ... end
print(va(), va(nil), va(1, nil, 3), (va(1, 2)))
local function named(a, ...rest) return a, #rest, rest.n, rest[1], rest[#rest] end
print(named(1, 2, 3, 4))
print(select(-1, "a", "b", "c"), select(2, "a", "b", "c"))
local function tail(n) if n == 0 then return "done" end return tail(n - 1) end
print(tail(100000))
print(pcall(select, 0))
