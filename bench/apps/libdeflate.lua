-- LibDeflate compresses and decompresses the Teal compiler's source.
-- usage: libdeflate.lua ROOT LEVEL [KIB]
local root, level, kib = arg[1], tonumber(arg[2]), tonumber(arg[3])
local LibDeflate = dofile(root .. "/libdeflate/LibDeflate.lua")
local f = assert(io.open(root .. "/tl/tl.lua", "rb")); local data = f:read("a"); f:close()
if kib then data = data:sub(1, kib * 1024) end
local c = LibDeflate:CompressDeflate(data, {level = level})
assert(LibDeflate:DecompressDeflate(c) == data, "round trip")
print(#data .. " -> " .. #c)
