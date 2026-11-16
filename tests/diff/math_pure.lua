-- The math functions with generated fast paths, called with every kind of
-- argument: the fast path takes only exact types and gives up on the rest,
-- so each call must match the full function's result or error.

local names = {
  "floor", "ceil", "abs", "sqrt", "exp", "sin", "cos", "tan", "asin",
  "acos", "deg", "rad", "atan", "log", "ult", "ldexp",
}
if math.cosh then
  for _, n in ipairs({"cosh", "sinh", "tanh", "log10", "pow"}) do
    names[#names + 1] = n
  end
end

local values = {
  0, -0.0, 1, -7, 3.5, -2.5, 1e308, -1e308, 1 / 0, -1 / 0,
  math.maxinteger, math.mininteger, 2^53, 3.0, "2", " 0x10 ", "x", true,
}

local function show(v)
  if v ~= v then
    return "nan"
  elseif math.type(v) == "float" then
    return string.format("%.17g", v)
  else
    return tostring(v)
  end
end

local function call(f, ...)
  local r = table.pack(pcall(f, ...))
  local out = {}
  for i = 1, r.n do
    out[i] = show(r[i])
  end
  return r.n .. ": " .. table.concat(out, " ")
end

for _, n in ipairs(names) do
  local f = math[n]
  print(n, "()", call(f))
  print(n, "(nil)", call(f, nil))
  for _, a in ipairs(values) do
    print(n, show(a), call(f, a))
    print(n, show(a), "nil", call(f, a, nil))
    print(n, show(a), "2", call(f, a, 2))
    print(n, show(a), "1.5", call(f, a, 1.5))
    print(n, show(a), "x", call(f, a, "x"))
  end
  -- Wanting no results, one, and all of them.
  f(4, 2)
  local one = f(4, 2)
  print(n, "want", show(one), select("#", f(4, 2)), select("#", f(4, 2, 3)))
end
-- A NaN argument.
print(math.floor(0 / 0) ~= math.floor(0 / 0), math.abs(0 / 0) ~= math.abs(0 / 0))
