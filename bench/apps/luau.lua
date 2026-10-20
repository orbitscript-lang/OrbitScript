-- One program from Luau's benchmark suite (bench/tests), those that run on
-- plain Lua. They find bench_support.lua here through package.path.
-- usage: luau.lua ROOT TEST
local root, test = arg[1], arg[2]
arg = {[0] = test} -- the tests read no arguments; scimark would take ours as options
dofile(root .. "/luau/bench/tests/" .. test)
