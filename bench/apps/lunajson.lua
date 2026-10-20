-- lunajson encodes generated records and decodes them five times.
-- usage: lunajson.lua ROOT RECORDS
local root, n = arg[1], tonumber(arg[2])
package.path = root .. "/lunajson/src/?.lua;" .. package.path
local lj = require("lunajson")
local rows = {}
for i = 1, n do
  rows[i] = {id = i, name = "item" .. i, price = i * 1.25, tags = {"a", "b", tostring(i % 7)},
             nested = {ok = i % 2 == 0, ratio = i / 3, text = "line \"" .. i .. "\"\n"}}
end
local s = lj.encode(rows)
local back
for _ = 1, 5 do back = lj.decode(s) end
assert(#back == n and back[n].nested.text == rows[n].nested.text)
print(#s .. " bytes")
