-- Library error messages keep the bytes of the strings they quote, even
-- when those bytes aren't UTF-8. Bytes from 0x80 up print as <XX>.
local function esc(v)
  return (string.gsub(tostring(v), "[\128-\255]", function (c)
    return string.format("<%02X>", c:byte())
  end))
end

local function show(...)
  local r = table.pack(pcall(...))
  for i = 1, r.n do r[i] = esc(r[i]) end
  print(table.unpack(r, 1, r.n))
end

show(string.format, "%\255", 1)
show(string.format, "%10.123\255", 1)
show(string.format, "%1\255", 1)
show(string.pack, "\255")
show(string.unpack, "\233", "")
show(os.date, "%\255")
show(os.setlocale, nil, "\255")
show(io.lines, "/nonexistent/\255")
show(loadfile, "/nonexistent/\255")
show(loadfile, arg[0], "\255")
show(collectgarbage, "\255")
show(collectgarbage, "param", "\255")
show(function () io.tmpfile():seek("\255") end)
show(function () io.tmpfile():setvbuf("\255") end)
local t = {["f\255"] = string.rep}
show(function () t["f\255"]() end)
show(function () return ("x"):rep({}) end)
