-- Multiple assignment: every value is computed before any target changes, however the
-- compiler places the values.

local function show(...)
  local t = table.pack(...)
  for i = 1, t.n do t[i] = tostring(t[i]) end
  print(table.concat(t, " "))
end

-- Swaps and rotations.
do
  local a, b, c = 1, 2, 3
  a, b = b, a
  show(a, b)
  a, b, c = b, c, a
  show(a, b, c)
  a, b, c = c, a, b
  show(a, b, c)
  a, b = b + 10, a + 20
  show(a, b)
  a, b = a + b, a - b
  show(a, b)
end

-- A target read by a later value, an earlier one, or its own.
do
  local x, y, z = 1, 2, 3
  x, y, z = y, z, x
  show(x, y, z)
  x, y, z = x + 1, x + 2, x + 3
  show(x, y, z)
  x, y, z = z * 2, y * 2, x * 2
  show(x, y, z)
  x, y = -x, x
  show(x, y)
  x, y = #"abc" + y, (x)
  show(x, y)
end

-- Table targets whose object or key is assigned in the same statement.
do
  local t = {10, 20, 30}
  local i = 1
  i, t[i] = i + 1, 99
  show(i, t[1], t[2], t[3])
  t = {10, 20, 30}
  i = 1
  t[i], i = i, t[i]
  show(i, t[1], t[2], t[10])
  local u = {}
  local s = t
  t, t.x = u, 5
  show(t == u, s.x, u.x)
  local k = "a"
  t[k], k = k, "b"
  show(k, t.a, t.b)
  k, t[k] = "c", k
  show(k, t.b, t.c)
end

-- Plain locals stored into fields straight from their registers.
do
  local o = {}
  local a, b, c = 1, 2, 3
  o.a, o.b, o.c = a, b, c
  show(o.a, o.b, o.c)
  o.a, a = a + 100, o.a
  show(o.a, a)
  o[1], o[2], b = b, c, a
  show(o[1], o[2], b)
  a, o.z = 7, a
  show(a, o.z)
end

-- Repeated targets: the leftmost assignment wins, as in the reference.
do
  local a = 0
  a, a = 1, 2
  show(a)
  local b, c = 0, 0
  b, c, b = 1, 2, 3
  show(b, c)
end

-- Captured locals live in cells; closures see the new values only after the statement.
do
  local a, b = 1, 2
  local function get() return a, b end
  a, b = b, a
  show(get())
  a, b = get()
  show(a, b)
  local seen
  local mt = {__add = function(x, y) seen = {get()} return 0 end}
  local o = setmetatable({}, mt)
  a, b = 5, o + 1
  show(a, b, seen[1], seen[2])
end

-- Metamethods run while values are computed; their order and results are unchanged.
do
  local log = {}
  local mt = {
    __sub = function(x, y) log[#log + 1] = "sub" .. tostring(y) return y end,
    __index = function(t, k) log[#log + 1] = "get" .. k return k end,
    __newindex = function(t, k, v) log[#log + 1] = "set" .. k .. "=" .. tostring(v) rawset(t, k, v) end,
  }
  local o = setmetatable({}, mt)
  local p, q, r = 1, 2, 3
  p, q, r = o - 1, o.q, o - 3
  show(p, q, r)
  o.x, o.y, p = p, q, r
  show(p, rawget(o, "x"), rawget(o, "y"))
  print(table.concat(log, " "))
end

-- Calls and varargs on the right, adjusted or not.
do
  local function two() return 1, 2 end
  local a, b, c = 0, 0, 0
  a, b = two()
  show(a, b)
  a, b, c = 9, two()
  show(a, b, c)
  a, b = two(), two()
  show(a, b)
  a, b = (two())
  show(a, b)
  a, b, c = c, b
  show(a, b, c)
  a, b = 1, 2, two()
  show(a, b)
  local function va(...)
    local x, y = 0, 0
    x, y = ..., x
    return x, y
  end
  show(va(5, 6))
end

-- `and`, `or` and table constructors that read their own target.
do
  local a, b = 1, false
  a, b = b or a, a and b
  show(a, b)
  a, b = {a, b}, a
  show(type(a), a[1], a[2], b)
  local t = {1}
  t, b = {t[1] + 1, t}, #t
  show(t[1], type(t[2]), b)
end

-- Globals and upvalues mixed with locals.
do
  g1, g2 = 1, 2
  local l = 3
  g1, l, g2 = l, g2, g1
  show(g1, l, g2)
end

-- Errors while computing a value name the right operand.
do
  local x, y = 1, nil
  local ok, err = pcall(function()
    local a, b = 1, 2
    a, b = b + 1, y + a
    return a, b
  end)
  print(ok, (err:gsub("^.-:%d+: ", "")))
  ok, err = pcall(function()
    local a, b, c = 1, 2, nil
    a, b = c + 1, a
  end)
  print(ok, (err:gsub("^.-:%d+: ", "")))
  ok, err = pcall(function()
    local a, b = 1, {}
    a, b = b.x.y, 2
  end)
  print(ok, (err:gsub("^.-:%d+: ", "")))
  print(x, y)
end

-- Loops that reassign their state, as in numeric kernels.
do
  local vx, vy = 1.5, -2.5
  for i = 1, 4 do
    local dx, dy = i * 0.5, -i * 0.25
    vx, vy = vx - dx * vy, vy - dy * vx
  end
  show(vx, vy)
  local fa, fb = 0, 1
  for _ = 1, 20 do fa, fb = fb, fa + fb end
  show(fa, fb)
end
