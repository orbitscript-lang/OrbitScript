local which, N = arg[1], tonumber(arg[2] or 10000000)
package.cpath = "./?.so;" .. package.cpath
local f, a
local S = "abcdefghijklmnopqrst"
if which == "empty" then f = false; a = 1
elseif which == "loop" then f = function(x) return x end; a = 1
elseif which == "abs.lua" then f = function(x) if x < 0 then return -x end return x end; a = -5
elseif which == "abs.math" then f = math.abs; a = -5
elseif which == "abs.native" then f = ffi.native_labs; a = -5
elseif which == "abs.ffi" then f = ffi.fn(ffi.open("libc.so.6"), "labs", "long", "long"); a = -5
elseif which == "abs.ffi_closure" then f = ffi.fn_closure(ffi.open("libc.so.6"), "labs", "long", "long"); a = -5
elseif which == "abs.ffi_generic" then f = ffi.fn_generic(ffi.open("libc.so.6"), "labs", "long", "long"); a = -5
elseif which == "abs.closure" then f = ffi.closure_labs(); a = -5
elseif which == "abs.cmod" then f = require("cbench").labs; a = -5
elseif which == "len.string" then f = string.len; a = S
elseif which == "len.native" then f = ffi.native_strlen; a = S
elseif which == "len.ffi" then f = ffi.fn(ffi.open("libc.so.6"), "strlen", "size", "cstr"); a = S
elseif which == "len.ffi_long" then f = ffi.fn(ffi.open("libc.so.6"), "strlen", "size", "cstr"); a = string.rep("x", 1000)
elseif which == "len.native_long" then f = ffi.native_strlen; a = string.rep("x", 1000)
elseif which == "len.cmod" then f = require("cbench").strlen; a = S
elseif which == "sqrt.math" then f = math.sqrt; a = 2.0
elseif which == "sqrt.ffi" then f = ffi.fn(ffi.open("libm.so.6"), "sqrt", "double", "double"); a = 2.0
elseif which == "sqrt.cmod" then f = require("cbench").sqrt; a = 2.0
elseif which == "add3.lua" then f = function(x, y, z) return x + y + z end
elseif which == "add3.ffi" then f = ffi.fn(ffi.open("./cbench.so"), "add3", "long", "long", "long", "long")
elseif which == "add3.cmod" then f = require("cbench").add3
else error("unknown " .. tostring(which)) end
local t0 = os.clock()
local r
if not f then
  for i = 1, N do r = a end
elseif which:match("^add3") then
  for i = 1, N do r = f(i, 2, 3) end
else
  for i = 1, N do r = f(a) end
end
io.write(string.format("%.1f\n", (os.clock() - t0) * 1e9 / N))
