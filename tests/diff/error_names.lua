-- Error messages keep the bytes of names a script gave (`__name`, field names) even when
-- they are not UTF-8. The bytes are shown escaped so the output stays text.
local mt = {__name = "\xff\xfename"}
local x = setmetatable({}, mt)
local function try(f, ...) local _, e = pcall(f, ...) print((string.gsub(e, "[\x80-\xff]", function(c) return ("<%02x>"):format(c:byte()) end))) end
try(function() return x + 1 end)
try(function() return x & 1 end)
try(function() return x < x end)
try(function() return x < 1 end)
try(function() return #setmetatable({}, {__name = "\xfe"}) end)
try(function() return x .. "a" end)
try(function() return x() end)
try(function() return x.y.z end)
try(function() for i = x, 2 do end end)
try(string.rep, x)
try(math.floor, x)
try(function() local t = {} t["\xff"] = 1.5 return 1 | t["\xff"] end)
try(function() local t = {} return t["\xffk"].z end)
