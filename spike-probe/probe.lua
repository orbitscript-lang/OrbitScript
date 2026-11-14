-- Line-ending probe: run under lua5.5, lua5.4 and orbit on Windows and diff.
-- Usage: <lua> probe.lua files <dir>   -- file cases, report to <dir>/report.txt
--        <lua> probe.lua stdout        -- writes test bytes to stdout
--        <lua> probe.lua stderr        -- writes test bytes to stderr
--        <lua> probe.lua stdin <dir>   -- reads stdin, report to <dir>/stdin.txt
--        <lua> probe.lua popen <dir>   -- io.popen cases, report to <dir>/popen.txt
-- Reports hold printable bytes as-is and the rest as <hex>, one case per line.
local mode, dir = arg[1], arg[2]

local function show(v)
  if v == nil then return "nil" end
  if type(v) ~= "string" then return tostring(v) end
  return (v:gsub("[^%w%p ]", function(c) return ("<%02x>"):format(c:byte()) end))
end

local report
local function rec(name, ...)
  local t = table.pack(...)
  local parts = {}
  for i = 1, t.n do parts[#parts + 1] = show(t[i]) end
  report:write(name, " = ", table.concat(parts, " | "), "\n")
end

local function path(n) return dir .. "\\" .. n end
local function raw_write(n, bytes)
  local f = assert(io.open(path(n), "wb")); f:write(bytes); f:close()
end
local function raw_read(n)
  local f = assert(io.open(path(n), "rb")); local s = f:read("a"); f:close(); return s
end
local function try(name, fn)
  local ok, err = pcall(fn)
  if not ok then rec(name .. " ERROR", err) end
end

local W = "a\nb\r\nc\rd\26e\n"           -- what we write
local R = "l1\r\nl2\n\r\nl3\rX\26after\r\nend" -- what we read

if mode == "files" then
  report = assert(io.open(path("report.txt"), "wb"))
  for _, m in ipairs({ "w", "wb", "w+", "w+b" }) do
    try("write " .. m, function()
      local f = assert(io.open(path("w.bin"), m))
      f:write(W)
      rec("write " .. m .. " seek(cur) after write", f:seek("cur"))
      if m:find("+", 1, true) then
        f:seek("set", 0); rec("write " .. m .. " read back", f:read("a"))
      end
      f:close()
      rec("write " .. m .. " bytes on disk", raw_read("w.bin"))
    end)
  end
  for _, m in ipairs({ "a", "ab", "a+", "a+b" }) do
    try("append " .. m, function()
      raw_write("a.bin", "x\26")
      local f = assert(io.open(path("a.bin"), m)); f:write("y\n"); f:close()
      rec("append " .. m .. " to x<1a>", raw_read("a.bin"))
    end)
  end
  try("io.output", function()
    io.output(path("o.bin")); io.write("o\n"); io.close(); io.output(io.stdout)
    rec("io.output(name) write o\\n", raw_read("o.bin"))
  end)
  try("setvbuf no", function()
    local f = assert(io.open(path("v.bin"), "w")); f:setvbuf("no"); f:write("v\n", "w\r\n"); f:close()
    rec("setvbuf no text write", raw_read("v.bin"))
  end)
  raw_write("r.bin", R)
  for _, m in ipairs({ "r", "rb" }) do
    try("read " .. m, function()
      local f = assert(io.open(path("r.bin"), m))
      rec("read " .. m .. " a", f:read("a"))
      rec("read " .. m .. " a again", f:read("a"))
      rec("read " .. m .. " seek(end)", f:seek("end"))
      f:seek("set", 0)
      local ls = {}
      for _ = 1, 8 do ls[#ls + 1] = show(f:read("l")) .. "@" .. tostring(f:seek("cur")) end
      rec("read " .. m .. " l x8 (value@pos)", table.concat(ls, ","))
      f:seek("set", 0)
      local Ls = {}
      for _ = 1, 8 do Ls[#Ls + 1] = show(f:read("L")) end
      rec("read " .. m .. " L x8", table.concat(Ls, ","))
      f:seek("set", 0)
      rec("read " .. m .. " read(3),read(3),read(3)", f:read(3), f:read(3), f:read(3))
      f:seek("set", 2); rec("read " .. m .. " seek(set,2) read(2)", f:read(2))
      f:seek("set", 3); rec("read " .. m .. " seek(set,3) read(2)", f:read(2))
      f:seek("set", 0); rec("read " .. m .. " read(0) at start", f:read(0))
      f:seek("set", 13); rec("read " .. m .. " read(0) at <1a>", f:read(0), f:read(1))
      f:close()
    end)
  end
  try("io.lines", function()
    local t = {}
    for l in io.lines(path("r.bin")) do t[#t + 1] = show(l) end
    rec("io.lines(name)", table.concat(t, ","))
    t = {}
    for l in io.lines(path("r.bin"), "L") do t[#t + 1] = show(l) end
    rec("io.lines(name, L)", table.concat(t, ","))
    local f = assert(io.open(path("r.bin"), "rb")); t = {}
    for l in f:lines() do t[#t + 1] = show(l) end
    f:close()
    rec("rb f:lines()", table.concat(t, ","))
  end)
  try("io.input", function()
    io.input(path("r.bin")); rec("io.input(name) io.read(a)", io.read("a")); io.close(); io.input(io.stdin)
  end)
  try("numbers", function()
    raw_write("n.bin", "12\r\n34\r\n\26 56")
    local f = assert(io.open(path("n.bin"), "r"))
    rec("read r n,n,n", f:read("n", "n", "n")); f:close()
  end)
  try("cr edges", function()
    raw_write("c.bin", "a\r\r\nb\r")
    local f = assert(io.open(path("c.bin"), "r")); rec("read r a\\r\\r\\nb\\r", f:read("a")); f:close()
    raw_write("c.bin", "\r\n")
    f = assert(io.open(path("c.bin"), "r")); rec("read r just \\r\\n: l,l", f:read("l"), f:read("l")); f:close()
  end)
  try("big", function()
    -- a CRLF straddling the C library's buffer boundary
    local s = ("x"):rep(4095) .. "\r\n" .. ("y"):rep(4094) .. "\r\nz"
    raw_write("big.bin", s)
    local f = assert(io.open(path("big.bin"), "r"))
    local got = f:read("a"); f:close()
    rec("read r 4096-boundary len, crlf left", #got, select(2, got:gsub("\r", "")))
  end)
  try("tmpfile", function()
    local f = io.tmpfile(); f:write("t\n"); f:seek("set", 0)
    rec("tmpfile write t\\n read back", f:read("a"), f:seek("end")); f:close()
  end)
  try("loadfile", function()
    raw_write("s1.lua", "local x = 1\r\nreturn x + 1\r\n")
    rec("loadfile CRLF script", pcall(dofile, path("s1.lua")))
    raw_write("s2.lua", "return 'a\26b'")
    rec("loadfile <1a> in string", loadfile(path("s2.lua")))
    raw_write("s3.lua", "return 1\26 garbage that does not parse")
    rec("loadfile code then <1a> then junk", loadfile(path("s3.lua")))
    raw_write("s4.lua", "return [[x\r\ny\rz]]")
    local fn = loadfile(path("s4.lua")); rec("loadfile long string CRLF/CR", fn and fn())
    raw_write("s5.lua", "#!shebang\r\nreturn 5")
    fn = loadfile(path("s5.lua")); rec("loadfile shebang CRLF", fn and fn())
    local bin = string.dump(function() return "q\r\n\26" end)
    raw_write("s6.bin", bin)
    fn = loadfile(path("s6.bin")); rec("loadfile binary chunk", fn and fn())
    raw_write("s7.bin", "#x\n" .. bin)
    fn = loadfile(path("s7.bin")); rec("loadfile #line + binary chunk", fn and fn())
  end)
  report:close()
elseif mode == "stdout" then
  io.write("w1\n"); io.write("w2\r\n"); io.write("w3\r"); io.write("w4\26z\n")
  print("p1"); print("p2\r")
  io.stdout:write("s1\n")
  io.stdout:setvbuf("no"); io.stdout:write("u1\n")
  io.stdout:setvbuf("line"); io.stdout:write("u2\n")
  io.stdout:setvbuf("full"); io.stdout:write("u3\n")
  io.stdout:write(string.char(10, 10, 13, 10))
elseif mode == "stderr" then
  io.stderr:write("e1\n"); io.stderr:write("e2\r\n")
  warn("@on"); warn("w", "x")
  error("boom")
elseif mode == "stdin" then
  report = assert(io.open(path("stdin.txt"), "wb"))
  rec("stdin l", io.read("l"))
  rec("stdin L", io.read("L"))
  rec("stdin 3", io.read(3))
  rec("stdin a", io.read("a"))
  rec("stdin a again", io.read("a"))
  report:close()
elseif mode == "popen" then
  report = assert(io.open(path("popen.txt"), "wb"))
  raw_write("r.bin", R)
  local ps = 'powershell -NoProfile -Command "'
  local function q(n) return "'" .. path(n) .. "'" end
  local cat = ps .. "$o=[Console]::OpenStandardOutput();$b=[IO.File]::ReadAllBytes(" .. q("r.bin")
    .. ");$o.Write($b,0,$b.Length);$o.Flush()\""
  local function sink(n)
    return ps .. "$i=[Console]::OpenStandardInput();$o=[IO.File]::Create(" .. q(n)
      .. ");$i.CopyTo($o);$o.Close()\""
  end
  try("popen r", function()
    local p = assert(io.popen("echo hi", "r")); rec("popen r echo hi", p:read("a")); p:close()
    p = assert(io.popen(cat, "r")); rec("popen r raw r.bin a", p:read("a")); p:close()
    p = assert(io.popen(cat, "r")); local t = {}
    for l in p:lines() do t[#t + 1] = show(l) end
    p:close(); rec("popen r raw r.bin lines", table.concat(t, ","))
  end)
  try("popen w", function()
    local p = assert(io.popen(sink("pw.bin"), "w"))
    p:write(W); p:close()
    rec("popen w raw bytes received", raw_read("pw.bin"))
  end)
  report:close()
end
