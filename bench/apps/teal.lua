-- The Teal compiler (single-file tl.lua) type-checks and compiles Teal's own
-- sources, sharing one environment. usage: teal.lua ROOT [NFILES]
local root, limit = arg[1], tonumber(arg[2])
local tl = dofile(root .. "/tl/tl.lua")
local env = tl.new_env()
local files, bytes, errors = 0, 0, 0
for name in io.lines("lists/teal.txt") do
  if limit and files == limit then break end
  local f = assert(io.open(root .. "/tl/" .. name)); local src = f:read("a"); f:close()
  local out, result = tl.gen(src, env)
  files, bytes = files + 1, bytes + #(out or "")
  errors = errors + #(result and result.syntax_errors or {}) + #(result and result.type_errors or {})
end
print(files .. " files, " .. bytes .. " bytes of Lua, " .. errors .. " errors")
