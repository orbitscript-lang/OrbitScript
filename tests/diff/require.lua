-- require and the package library. Modules live in tests/diff/modules;
-- the path is set relative to this file's directory.

local function show(...)
  local t = {...}
  for i = 1, select("#", ...) do t[i] = tostring(t[i]) end
  print(select("#", ...), table.concat(t, " | "))
end

local dir = string.sub(arg[0], 1, #arg[0] - #"require.lua") .. "modules/"
package.path = dir .. "?.lua;" .. dir .. "?/init.lua"
package.cpath = dir .. "?.so"

-- Standard libraries are preloaded.
show(require("string") == string, require("math") == math, require("table") == table)
show(require("os") == os, require("_G") == _G, require("package") == package)
show(package.loaded._G == _G, type(package.preload), #package.searchers)
show(package.config)

-- Loading, caching, the loader's arguments.
local a, extra = require("mod_a")
show(a.name, a.file == dir .. "mod_a.lua", extra == a.file, count)
show(require("mod_a") == a, count)
show(require("mod_nil"), package.loaded.mod_nil, loaded_nil)
show(require("mod_nil"), loaded_nil)
show(require("pkg"))
local sub, subfile = require("pkg.sub")
show(sub == "pkg.sub: " .. subfile, subfile == dir .. "pkg/sub.lua")

-- Errors.
local ok, msg = pcall(require, "mod_err")
show(ok, msg)
ok, msg = pcall(require, "mod_syntax")
show(ok, string.sub(msg, 1, 37))
show(pcall(require, "no_such_module"))
show(pcall(require))
show(pcall(require, {}))

-- preload and searchers.
package.preload.pre = function (...) return {...} end
local p, pextra = require("pre")
show(p[1], p[2], pextra)
package.preload.prenil = function () end
show(require("prenil"))

-- Replacing package fields: loaded and preload are kept by require.
local saved_loaded, saved_path = package.loaded, package.path
package.loaded = nil
show(require("string") == string)
package.loaded = saved_loaded
package.path = 3
show(pcall(require, "zz"))
package.path = {}
local ok2, msg2 = pcall(require, "zz2")
show(ok2, msg2)
package.path = saved_path

local searchers = package.searchers
package.searchers = 3
show(pcall(require, "a"))
package.searchers = searchers
local s1, s2 = searchers[1], searchers[2]
searchers[1] = function () return "msg1" end
searchers[2] = function () return 33 end
show(pcall(require, "zzz"))
searchers[1], searchers[2] = s1, s2

-- searchpath.
show(package.searchpath("a.b.c", "?;x/?.lua"))
show(package.searchpath("a.b", "?", "", ""))
show(package.searchpath("x", "a;;b"))
show(package.searchpath("x", ";"))
show(package.searchpath("x", ""))
show(package.searchpath("--x-", "?", "-", "X"))
show(package.searchpath("mod_a", dir .. "?.lua") == dir .. "mod_a.lua")
show(pcall(package.searchpath))

-- No dynamic libraries.
local f, err, when = package.loadlib("donotexist", "xuxu")
show(f, type(err), when == "absent" or when == "open")
