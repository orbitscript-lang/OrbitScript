local function P(...) local r = {pcall(...)} for i = 1, #r do r[i] = tostring(r[i]) .. ":" .. (math.type(r[i]) or type(r[i])) end print(table.concat(r, " ")) end
P(math.fmod, 5, 0) P(math.fmod, 5.0, 0) P(math.fmod, -6, 4) P(math.fmod, math.mininteger, -1) P(math.fmod, 6, -4.0) P(math.fmod, 1, math.mininteger) P(math.fmod, "6", 4) P(math.fmod, -6.0, 4) P(math.fmod, 3, 2^64)
P(math.tointeger, 3.0) P(math.tointeger, "8") P(math.tointeger, 3.5) P(math.tointeger, {}) P(math.tointeger) P(math.tointeger, "3.0") P(math.tointeger, 2^63)
P(math.floor, "3.7") P(math.ceil, -0.5) P(math.ceil, 2^70) P(math.floor, -2^63) P(math.floor, -0.0) P(math.floor, 0/0) P(math.ceil, 1/0) P(math.floor, "3")
P(math.modf, 3.7) P(math.modf, -3.7) P(math.modf, 1/0) P(math.modf, 5) P(math.modf, 0/0) P(math.modf, -0.5) P(math.modf, 2^70) P(math.modf, -1/0)
P(math.log, 8, 2) P(math.log, 100, 10) P(math.log, 27, 3) P(math.log, 0) P(math.log, 2) P(math.ult, 1, -1) P(math.ult, 1.0, 2) P(math.ult, 1.5, 2)
P(math.abs, math.mininteger) P(math.abs, "-3") P(math.abs, -0.0) P(math.atan, 1, -1) P(math.atan, 1) P(math.max) P(math.min, 1, "2") P(math.exp, 1) P(math.sqrt, -1)
P(math.max, "a", "b") P(math.max, {}, {}) P(math.max, 1, {}) P(math.max, 1, 2.0) P(math.max, 2, 2.0) P(math.max, 2.0, 2) P(math.min, -0.0, 0.0) P(math.max, 0/0, 1) P(math.max, 1, 0/0)
P(math.random, 1, 2, 3) P(math.random, 0.5) P(math.random, 3, 1) P(math.random, 0, 0) P(math.random, -1)
math.randomseed(42) P(math.random, 1, 6) P(math.random, 100) P(math.random) P(math.random, -10, 10) P(math.random, math.mininteger, math.maxinteger) P(math.random, 0)
for i = 1, 20 do P(math.random, 1, i * 7) end
P(math.randomseed, 7, 9) P(math.random, 0) P(math.random, 3, 3) P(math.random, math.maxinteger - 3, math.maxinteger)
P(math.randomseed, -5) P(math.random, 0) P(math.randomseed, 2^53) P(math.randomseed, 1, 2.5) P(math.randomseed, "x")
P(math.deg, math.pi) P(math.rad, 180) P(math.frexp, 8) P(math.frexp, 0) P(math.frexp, -3.5) P(math.frexp, 1/0) P(math.frexp, 5e-324) P(math.frexp, -0.0)
P(math.ldexp, 1, 3) P(math.ldexp, 0.5, 1.0) P(math.ldexp, 1, 1.5) P(math.ldexp, 1, -1074) P(math.ldexp, 1, -1075) P(math.ldexp, 0.75, -1074) P(math.ldexp, 1, 1024) P(math.ldexp, 1.5, -1073) P(math.ldexp, 3, -1076) P(math.ldexp, 2^-1074, 2000) P(math.ldexp, 1, math.mininteger)
P(math.sin) P(math.sin, 0) P(math.cos, math.pi) P(math.tan, 1) P(math.asin, 1) P(math.acos, 0) P(math.exp, "1") P(math.sqrt, 2)
P(math.type, 1) P(math.type, 1.0) P(math.type, "1") P(math.type)
math.randomseed(1007) P(math.random, 0) math.randomseed(1007, 0) P(math.random)

-- String-to-number edges.
for _, s in ipairs{"-9223372036854775808", "-9223372036854775809", "9223372036854775807"} do
  print(s, tonumber(s), math.type(tonumber(s)))
end
print(tonumber("  +1Z  ", 36), tonumber("+-1", 10), tonumber("+", 10), tonumber("-ff", 16))
