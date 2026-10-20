-- luacheck lints Penlight, luacheck and Teal sources from memory (no file
-- system access during the run). usage: luacheck.lua ROOT [NFILES]
local root, limit = arg[1], tonumber(arg[2])
package.path = root .. "/luacheck/src/?.lua;" .. root .. "/luacheck/src/?/init.lua;" .. package.path
local luacheck = require("luacheck")
local srcs = {}
for name in io.lines("lists/luacheck.txt") do
  if limit and #srcs == limit then break end
  local f = assert(io.open(root .. "/" .. name)); srcs[#srcs + 1] = f:read("a"); f:close()
end
local report = luacheck.check_strings(srcs)
print(#srcs .. " files, " .. report.warnings .. " warnings, " .. report.errors .. " errors")
