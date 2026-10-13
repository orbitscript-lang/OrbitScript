local next, type, getmetatable, rawget, select = next, type, getmetatable, rawget, select
local find, sub, sort, unpack = string.find, string.sub, table.sort, table.unpack
local G = _ENV
local keywords = {"and", "break", "do", "else", "elseif", "end", "false", "for",
  "function", "global", "goto", "if", "in", "local", "nil", "not", "or", "repeat",
  "return", "then", "true", "until", "while"}
local function index(v)
  local mt = getmetatable(v)
  return type(mt) == "table" and rawget(mt, "__index") or nil
end
local function field(v, k)
  for _ = 1, 16 do
    if type(v) == "table" then
      local r = rawget(v, k)
      if r ~= nil then return r end
    end
    v = index(v)
    if v == nil then return nil end
  end
end
return function(frag, ...)
  local v, out, seen = G, {}, {}
  for i = 1, select("#", ...) do
    v = field(v, (select(i, ...)))
    if v == nil then return end
  end
  local function add(k)
    if type(k) == "string" and not seen[k] and sub(k, 1, #frag) == frag
        and find(k, "^[%a_][%w_]*$") then
      seen[k] = true
      out[#out + 1] = k
    end
  end
  if select("#", ...) == 0 then
    for i = 1, #keywords do add(keywords[i]) end
  end
  for _ = 1, 16 do
    if type(v) ~= "table" then v = index(v) end
    if type(v) ~= "table" then break end
    for k in next, v do add(k) end
    v = index(v)
  end
  sort(out)
  return unpack(out, 1, #out < 200 and #out or 200)
end
