local function hex(s) if type(s) ~= "string" then return tostring(s) end local t = {} for i = 1, #s do t[#t+1] = string.format("%02x", string.byte(s, i)) end return table.concat(t) end
local function P(...) local r = {pcall(...)} for i = 1, #r do r[i] = hex(r[i]) end print(table.concat(r, " ")) end
local pack, unpack, packsize = string.pack, string.unpack, string.packsize
P(pack, "i0", 0) P(pack, "!17", 0) P(pack, "Xi17") P(pack, "i3r", 0) P(unpack, "i16", string.rep("\3", 16)) P(pack, "!4i3", 0) P(pack, "c", "")
P(packsize, "s") P(packsize, "c1" .. string.rep("0", 40)) P(packsize, string.format("c%dc10", math.maxinteger - 9)) P(pack, string.format("xxxxxxxxxx c%d", math.maxinteger - 9))
P(pack, "<I1", -1) P(pack, "s1", string.rep("a", 300)) P(pack, "z", "a\0") P(unpack, "zc10", "alo") P(unpack, "s", "\5\0\0\0\0\0\0\0ab") P(unpack, "c5", "abcd") P(pack, "s100", "x") P(pack, "c3", "1234") P(pack, "X") P(unpack, "c0", "abc", 5) P(unpack, "<I9", "\0\0\0\0\0\0\0\0\1") P(pack, "i", "x") P(pack, "i", 1.5) P(pack, "d", "x") P(pack, "i") P(unpack, "i", "abc")
P(packsize, "!i3") P(packsize,"!2i3") P(packsize, "!8 xXi16") P(packsize, "xXd") P(packsize, "!xXj") P(packsize, "!xd") P(packsize, "!2xd") P(packsize, "!xi16") P(packsize, "!4xi8")
P(unpack, "i4", pack("i4", 1), 1.0) P(unpack, "i4", "abcd", 1.5) P(pack, "f", 0.1)
P(pack, "Xz", 1) P(pack, "Xs", 1) P(pack, "Xc2") P(packsize, "!Xi16x") P(packsize, "!4 XI3") P(packsize, "!xX ") P(packsize, "! xXi16")
P(unpack, "z", "abc\0", 1) P(unpack, "<i3", "\255\255\127") P(unpack, "I3", "\255\255\255")
P(pack, "i1", 128) P(pack, "i1", -129) P(pack, "I8", -1) P(pack, "<i16", -2) P(pack, ">I10", -1)
P(unpack, "<I16", string.rep("\255",8)..string.rep("\0",8)) P(unpack, "<I16", string.rep("\255",9)..string.rep("\0",7)) P(unpack, "<i16", string.rep("\255",16)) P(unpack, "<i9", string.rep("\255",8).."\127")
P(unpack, "n j", pack("n j", 1.5, 3)) P(pack, "j", 2^63) P(pack, "j", 2^53) P(unpack, "f", pack("f", 1e300))
P(unpack, "i4", "abcdefgh", -100) P(unpack, "i4", "abcdefgh", 0) P(unpack, "i4", "abcd", 9) P(pack, "s", 5) P(pack, "z", 5) P(pack, "c2", 5)
P(pack, ">!8 b Xh i4 i8 c1 Xi8", -12, 100, 200, "\xEC") P(unpack, ">!8 c1 Xh i4 i8 b Xi8 XI XH", pack(">!8 b Xh i4 i8 c1 Xi8", -12, 100, 200, "\xEC"))
P(pack, ">!4 c3 c4 c2 z i4 c5 c2 Xi4", "abc", "abcd", "xz", "hello", 5, "world", "xy")
P(unpack, "!4 z c3", "abcdefghi\0xyz") P(unpack, "!4 i4", pack("i4i4i4i4", 1, 2, 3, 4), -7)
P(pack, "<b h b f d f n i", 1, 2, 3, 4, 5, 6, 7, 8) P(pack, "z") P(pack, "i1 i1", 1) P(unpack, "i") P(packsize) P(pack, ">s2 <s3", "ab", "cd")
P(pack, "<d >d =f", 1/0, -0.0, 0/0)
