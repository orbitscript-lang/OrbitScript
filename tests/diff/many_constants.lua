-- Functions with more constants than a 16-bit operand reaches: the ones
-- past that are loaded, indexed, assigned and called all the same, and
-- errors still name them (a missing method is the "method calls are
-- always named as methods" deviation, so it is not here).
local parts = {"local x"}
for i = 1, 70000 do parts[#parts + 1] = string.format("x = %d.5", i) end
local filler = table.concat(parts, "\n") .. "\n"

local function run(code)
  local f = assert(load(filler .. code, "=many"))
  print(pcall(f))
end

run[[
local t = {wide_field = 1, wide_method = function(self, x) return self.wide_field + x end}
t.wide_field = t.wide_field + 41
wide_global = "global " .. 1.25
local fields = {wide_key = 2.75}
fields.wide_key2, fields["wide_key3"] = 3, 4
wide_a, wide_b = "a", "b"
return t.wide_field, t:wide_method(0.5), wide_global, fields.wide_key,
  fields.wide_key2, fields.wide_key3, wide_a, wide_b, 123456.75, 2^53
]]
run"local t = {} return t.wide_missing.x"
run"return wide_undefined.x"
run"wide_undefined()"
run"local _ENV = {} return wide_global"
run"local _ENV = 1 return wide_global"
run"local _ENV = 1 wide_global = 1"

-- And they survive string.dump.
local f = assert(load(filler .. "wide_dumped = 'dumped' return wide_dumped, 99999.5", "=many"))
print(pcall(assert(load(string.dump(f)))))
