-- Table growth with integer keys in any order, nil holes in the array
-- part, borders (#) and next/pairs after deletes. Where a table has several
-- borders, only check that # returns one of them.

local function isborder(t, n)
  if n == 0 then return t[1] == nil end
  return t[n] ~= nil and t[n + 1] == nil
end

local function summary(t)
  local count, ksum, vsum = 0, 0, 0
  for k, v in pairs(t) do
    count = count + 1
    if math.type(k) == "integer" then ksum = ksum + k end
    if math.type(v) == "integer" then vsum = vsum + v end
  end
  return count, ksum, vsum
end

-- Integer keys 0..n-1 in a scattered order (a permutation: the stride is
-- coprime with n), then read back.
for _, n in ipairs({1, 2, 3, 7, 8, 9, 100, 1000, 4096, 5000}) do
  local t = {}
  for i = 1, n do t[(i * 7919) % n] = i end
  local ok = true
  for i = 1, n do
    if t[(i * 7919) % n] ~= i then ok = false end
  end
  print("perm", n, ok, #t, summary(t))
end

-- The same keys in descending and in interleaved order.
do
  local t = {}
  for i = 2000, 1, -1 do t[i] = i end
  print("desc", #t, summary(t))
  local u = {}
  for i = 1, 1000, 2 do u[i] = i end
  print("odd", isborder(u, #u), summary(u))
  for i = 2, 1000, 2 do u[i] = i end
  print("filled", #u, summary(u))
end

-- Deleting keys leaves holes; # stays a border and pairs sees exactly the
-- live keys.
do
  local n = 3000
  local t = {}
  for i = 1, n do t[(i * 7919) % n] = i end
  for i = 1, n, 2 do t[(i * 7919) % n] = nil end
  print("deleted", isborder(t, #t), summary(t))
  t[0] = nil
  print("zero", t[0], isborder(t, #t), summary(t))
  -- Refill the holes from the top down.
  for k = n - 1, 1, -1 do
    if t[k] == nil then t[k] = -k end
  end
  print("refilled", #t, summary(t))
  -- Delete everything except the last key: # is then 0 or n - 1.
  for k = 1, n - 2 do t[k] = nil end
  local b = #t
  print("sparse", b == 0 or b == n - 1, isborder(t, b), summary(t))
end

-- Clearing a table while traversing it, the only change next allows.
do
  local t = {}
  for i = 1, 500 do t[(i * 7) % 500 + 1] = i end
  for i = 1, 200 do t["s" .. i] = i end
  t[0], t[-5], t[2.5], t[1e300] = "z", "m", "f", "big"
  local seen = 0
  for k in pairs(t) do
    seen = seen + 1
    t[k] = nil
  end
  print("cleared", seen, next(t), #t)
  -- next on keys removed by the loop above still works for live tables.
  t[1], t[3] = "a", "c"
  local c1, k1 = summary(t)
  print("after clear", c1, k1, isborder(t, #t))
end

-- next after deletes, walked by hand, and with a key deleted mid-walk.
do
  local t = {}
  for i = 1, 64 do t[i * 3] = i end
  for i = 1, 64, 4 do t[i * 3] = nil end
  local count, ksum = 0, 0
  local k, v = next(t)
  while k do
    count, ksum = count + 1, ksum + k
    local nk = next(t, k)
    t[k] = nil
    k = nk
  end
  print("walk", count, ksum, next(t))
  print(pcall(next, t, 12345))
  print(pcall(next, {}, "missing"))
end

-- Keys that are not positive integers never join the array part.
do
  local t = {}
  for i = -50, 50 do t[i] = i end
  t[2.0], t[3.5], t["4"] = "two", "3.5", "four"
  print("mixed", #t, t[2], t[3.5], t["4"], t[4], t[-50], t[0], summary(t))
  for i = -50, 0 do t[i] = nil end
  print("mixed after", #t, summary(t))
end

-- Growth across powers of two with string keys forcing rebuilds.
do
  local t = {}
  for i = 1, 300 do
    t[i * 2] = i
    t["k" .. i] = i
  end
  for i = 1, 300 do t[i * 2 - 1] = -i end
  print("pow", #t, summary(t))
  for e = 1, 12 do
    local p = 1 << e
    print(e, t[p - 1] ~= nil, t[p] ~= nil, t[p + 1] ~= nil)
  end
end

-- ipairs stops at the first hole, wherever the keys live.
do
  local t = {}
  for i = 40, 1, -1 do t[i] = i end
  t[17] = nil
  local n = 0
  for i in ipairs(t) do n = i end
  print("ipairs", n, isborder(t, #t))
end

-- Constructors with nils and table functions on tables with one border.
do
  local t = {1, 2, nil, 4}
  print("ctor", isborder(t, #t), select("#", table.unpack(t, 1, 4)))
  local u = {}
  for i = 10, 1, -1 do u[i] = i end
  table.insert(u, 11)
  table.insert(u, 1, 0)
  print("insert", #u, u[1], u[12], table.concat(u, ","))
  print("remove", table.remove(u), table.remove(u, 1), #u)
  local s = {}
  for i = 1, 100 do s[(i * 37) % 100 + 1] = (i * 13) % 100 end
  table.sort(s)
  print("sort", #s, s[1], s[50], s[100])
end

-- Weak values in the array part are cleared to holes.
do
  local keep = {}
  local t = setmetatable({}, {__mode = "v"})
  for i = 64, 1, -1 do
    local v = {}
    t[i] = v
    if i % 2 == 0 then keep[#keep + 1] = v end
  end
  collectgarbage()
  collectgarbage()
  local count = summary(t)
  print("weak", count, isborder(t, #t), t[2] ~= nil, t[3])
end
