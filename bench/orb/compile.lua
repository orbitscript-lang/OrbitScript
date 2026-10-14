-- Times the OrbitScript front end: compiles desugar.orb (or the file given) N times.
-- usage: orbit bench/orb/compile.lua [N] [file.orb]
local n = tonumber(arg[1]) or 500
local path = arg[2] or (arg[0]:match("^(.*)/") or ".") .. "/desugar.orb"
local f = assert(io.open(path, "rb"))
local src = f:read("a")
f:close()
local name = "@" .. path
assert(load(src, name), "compile failed")
local t = os.clock()
for _ = 1, n do
  load(src, name)
end
local dt = os.clock() - t
print(string.format("compile %s: %d times, %.3f s, %.1f us each, %.1f MB/s",
  path, n, dt, dt / n * 1e6, #src * n / dt / 1e6))
