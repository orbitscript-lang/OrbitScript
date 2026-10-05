-- Weak tables and ephemerons (__mode). Output must not depend on pairs
-- order or on when automatic collection runs, so every check follows a
-- full collection and counts or looks up entries.
collectgarbage("incremental")

local function count(t)
  local n = 0
  for _ in pairs(t) do n = n + 1 end
  return n
end

-- Weak values: objects go, strings, numbers, booleans and natives stay.
local v = setmetatable({}, {__mode = "v"})
v[1] = "str"; v[2] = function () end; v[3] = {}; v[4] = print
v[5] = 1.5; v[6] = true; v.s = {}; v[string.rep("x", 50)] = {}
local kept = {}
v[7] = kept
collectgarbage()
local seen = {}
for i = 1, 7 do seen[#seen + 1] = tostring(v[i] ~= nil) end
print("weak values", table.concat(seen, " "), v.s, count(v))

-- Weak keys: object keys go with their entries; string and number keys
-- stay.
local k = setmetatable({}, {__mode = "k"})
for i = 1, 10 do k[{}] = i end
for i = 1, 10 do k[i] = i end
for i = 1, 10 do local s = string.rep("@", i); k[s] = s .. "#" end
local live = {}
k[live] = "live"
collectgarbage()
print("weak keys", count(k), k[live], k["@@@"])

-- All weak.
local kv = setmetatable({}, {__mode = "kv"})
local x, y = {}, {}
kv[1], kv[2] = x, y
kv[string.rep("$", 11)] = string.rep("$", 11)
for i = 3, 10 do kv[i] = {} end
for i = 1, 10 do kv[{}] = i end
for i = 1, 10 do local t = {}; kv[t] = t end
collectgarbage()
print("all weak", count(kv), kv[1] == x, kv[2] == y)
x, y = nil, nil
collectgarbage()
print("all weak after", count(kv), next(kv))

-- Ephemerons: a value is kept only through its key.
local e = setmetatable({}, {__mode = "k"})
do local key = {}; e[key] = {key} end           -- value refers to its key
do local k1, k2 = {}, {}; e[k1] = k2; e[k2] = k1 end  -- keys hold each other
local K = {}
e[K] = {}
collectgarbage()
print("ephemeron cycles", count(e), e[K] ~= nil)

-- A chain of ephemerons: each value holds the next key.
local chain = setmetatable({}, {__mode = "k"})
local first = {}
local key = first
for i = 1, 100 do
  local nextkey = {}
  chain[key] = {nextkey, i}
  key = nextkey
end
collectgarbage()
local n, at = 0, first
while chain[at] do n = n + 1; at = chain[at][1] end
print("chain", n)
first = nil
collectgarbage()
print("chain dropped", count(chain))

-- Ephemerons spread over several tables, as in gc.lua.
local mt = {__mode = "k"}
local a = setmetatable({}, mt)
local sub = {}
for i = 1, 10 do sub[i] = {}; a[sub[i]] = setmetatable({}, mt) end
local last = nil
local idx = 1
for j = 1, 100 do
  local node = {}
  local nk = idx % 10 + 1
  a[sub[nk]][node] = {last, k = idx}
  last = node
  idx = nk
end
collectgarbage()
local steps, node, kk = 0, last, idx
while node do
  local t = a[sub[kk]][node]
  node = t[1]; kk = t.k; steps = steps + 1
end
print("spread chain", steps)

-- __mode set after setmetatable takes effect; a non-string __mode is strong.
local m = {}
local t = setmetatable({}, m)
m.__mode = "k"
t[{}] = 1
collectgarbage()
print("mode later", next(t))
local strong = setmetatable({}, {__mode = 34})
strong[{}] = 1
collectgarbage()
print("non-string mode", count(strong))

-- A value whose weak value died: the key, still held, works with next.
local wv = setmetatable({}, {__mode = "v"})
local hk = {}
wv[hk] = {}
collectgarbage()
print("dead value", wv[hk], next(wv, nil), pcall(next, wv, hk))

-- Long string keys in all-weak tables, and dead entries.
local big = setmetatable({}, {__mode = "kv"})
big[string.rep("a", 1000)] = 25
big[string.rep("b", 1000)] = {}
big[{}] = 14
collectgarbage()
local bk, bv = next(big)
print("string key", bk == string.rep("a", 1000), bv, next(big, bk))
big[bk] = nil
bk = nil
collectgarbage()
print("emptied", next(big), big[string.rep("b", 100)])

-- false is still an ordinary key in a table with dead entries.
local f = setmetatable({}, {__mode = "k"})
for i = 1, 20 do f[{}] = i end
collectgarbage()
f[false] = "no"
f[{}] = nil
print("false key", f[false], count(f))
