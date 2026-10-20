-- Are We Fast Yet (Marr et al.): one benchmark, run once with its standard
-- self-check. usage: awfy.lua ROOT NAME INNER_ITERATIONS
local root, name, inner = arg[1], arg[2], tonumber(arg[3])
local dir = root .. "/awfy/benchmarks/Lua/"
package.path = dir .. "?.lua;" .. package.path
local bench = require(name:lower())
assert(bench:inner_benchmark_loop(inner), name .. ": wrong result")
print(name .. " " .. inner .. " ok")
