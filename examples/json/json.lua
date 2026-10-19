-- json: read, query, build and write JSON with the std.json module.
--
-- The Lua twin of json.orb: both print json.expected. Shows decoding a file,
-- json.null, integers and floats, json.object/json.array for empty tables and
-- arrays with holes, json.number for exact numerals, the scalar conversions,
-- keep_null,
-- encode's pretty and sort_keys options, a class that encodes as an array
-- through __tojson, and the errors decode returns and encode raises.
--
-- Usage: orbit json.lua [rounds]   (rounds > 1 repeats the work, for benchmarking)

local json = require "std.json"

local function read_file(path)
  local f = assert(io.open(path))
  local text = f:read("a")
  f:close()
  return text
end

-- Every product carrying `tag`, by name.
local function tagged(shop, tag)
  local names = {}
  for _, p in ipairs(shop.products) do
    for _, t in ipairs(p.tags) do
      if t == tag then names[#names + 1] = p.name end
    end
  end
  return names
end

-- The stock report: counts per tag and what to reorder. Built from scratch, so
-- the empty cases are marked: an empty `reorder` should still be an array and an
-- empty `tags` table an object.
local function report(shop)
  local tags = json.object()
  local reorder = json.array()
  local value = 0.0
  for _, p in ipairs(shop.products) do
    for _, t in ipairs(p.tags) do
      tags[t] = (tags[t] or 0) + 1
    end
    if p.stock == 0 and p.replaced_by == json.null then
      reorder[#reorder + 1] = p.sku
    end
    value = value + p.price * p.stock
  end
  return {shop = shop.name, tags = tags, reorder = reorder, stock_value = value}
end

-- A table encodes as an object of its fields unless its __tojson metamethod
-- gives something else to write: here the array [x, y].
local Point = {}
Point.__index = Point
function Point.new(x, y) return setmetatable({x = x, y = y}, Point) end
function Point:__tojson() return json.array({self.x, self.y}) end

local function run()
  local out = {}
  local function say(s) out[#out + 1] = s end

  -- Decoding: objects become tables with string keys, arrays sequences.
  local shop = assert(json.decode(read_file("shop.json")))
  say(string.format("%s (%s), opened %d, %d products",
    shop.name, shop.currency, shop.opened, #shop.products))
  for _, p in ipairs(shop.products) do
    -- Numbers keep their subtype: 24 is an integer, 189.5 a float.
    local price = math.type(p.price) == "integer"
      and string.format("%d", p.price) or string.format("%.2f", p.price)
    -- JSON null is json.null, never nil, so the field is still there.
    local status = p.replaced_by == json.null and "current"
      or "replaced by " .. p.replaced_by
    say(string.format("  %s %7s %-7s x%-2d %d tags, %-16s %s", p.sku, price,
      math.type(p.price), p.stock, #p.tags, status, p.name))
  end
  say("lights: " .. table.concat(tagged(shop, "light"), ", "))
  say("notes is " .. json.encode(shop.notes) .. ", tags of MAP-7 are "
    .. json.encode(shop.products[4].tags))

  -- Encoding, pretty-printed with sorted keys so the output is stable.
  say(json.encode(report(shop), {pretty = true, sort_keys = true}))

  -- Compact output; sort_keys makes it deterministic.
  local order = {id = 1001, lines = {{sku = "MAP-7", qty = 2}}, gift = false,
    note = json.null, ratio = 0.5, whole = 2.0}
  say(json.encode(order, {sort_keys = true}))

  -- What plain values can't say: an array with holes (here a week of visit
  -- counts) and numbers written exactly as given, beyond what a Lua number holds.
  local visits = json.array({}, 7)
  visits[1] = 3
  visits[5] = 1
  local receipt = {order = json.number("90071992547409930001"),
    total = json.number("44.90"), visits = visits}
  say(json.encode(receipt, {sort_keys = true}))
  -- Scalars write as their Lua type; json.int, float, string and bool convert
  -- (by the rules of OrbitScript's `as`), and bool follows conditions.
  say(json.encode({json.int(3.0), json.float(3), json.string(3), json.bool(0),
    json.bool(json.null)}))
  -- With keep_null off, null decodes to nil; the array keeps its length.
  local sparse = json.decode("[1, null, 3, null]", {keep_null = false})
  say(string.format("%s %s %s %s -> %s", sparse[1], sparse[2], sparse[3], sparse[4],
    json.encode(sparse)))

  -- Strings: escapes in, UTF-8 out, and the other way.
  local s = json.decode('"caf\\u00e9 \\u2603 tab\\tend"')
  say(s .. " (" .. #s .. " bytes)")
  say(json.encode("line\nbreak \"quoted\" \1"))

  -- Tables with number keys that aren't a sequence become objects.
  say(json.encode({[1] = "a", [3] = "c"}, {sort_keys = true}))
  -- A type can choose what it encodes as.
  say(json.encode({from = Point.new(0, 0), to = Point.new(3, 4)}, {sort_keys = true}))

  -- Round trip: decoding what encode wrote gives the same values back.
  local again = json.decode(json.encode(shop))
  assert(again.products[2].replaced_by == "LMP-2")
  assert(again.products[1].replaced_by == json.null)
  assert(math.type(again.products[2].price) == "integer")
  assert(again.products[1].price == 189.5)
  assert(json.encode(again.notes) == "{}")
  say("round trip ok")

  -- Errors: decode returns nil and a message; encode raises.
  for _, bad in ipairs({'{"a": 1,}', "[1, 2", "nul", '"\\x"'}) do
    local v, err = json.decode(bad)
    say(string.format("decode %-10s -> %s, %s", bad, tostring(v), err))
  end
  local loop = {}
  loop.self = loop
  for _, case in ipairs({
    {"a function", {handlers = {print}}},
    {"a cycle", loop},
    {"NaN", {ratio = 0 / 0}},
    {"a bad key", {[true] = 1}},
  }) do
    local ok, err = pcall(json.encode, case[2])
    say(string.format("encode %s -> %s", case[1], ok and "ok" or err))
  end
  say("json.null is " .. tostring(json.null) .. ", a " .. type(json.null))
  return table.concat(out, "\n")
end

local rounds = tonumber(arg and arg[1]) or 1
local text
for _ = 1, rounds do text = run() end
print(text)
