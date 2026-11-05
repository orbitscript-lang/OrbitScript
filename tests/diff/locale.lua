-- Locales: what os.setlocale changes in numbers, character classes,
-- case mapping, string comparison and dates. Each locale is tried only
-- when the machine has it, so the output depends on the installed
-- locales, the same way for both interpreters (CI generates pt_BR).

local function show(...)
  local t = {...}
  for i = 1, select("#", ...) do t[i] = tostring(t[i]) end
  print(select("#", ...), table.concat(t, " | "))
end

local function bytes(s)
  return table.concat({s:byte(1, -1)}, ",")
end

local function effects()
  show(tostring(3.5), 3.5 .. "", 1e100, -0.0, 2^63)
  show(string.format("%.2f %g %e %a %5.1f", 3.25, 0.5, 1.5, 1.0, -2.25))
  show(string.format("%q", 4.5), string.format("%d %x", 3, 255))
  show(tonumber("3,4"), tonumber("3.4"), tonumber(" 0x1p4 "), tonumber("1e2"))
  show(pcall(function() return "3,5" + 1 end))
  show(load("return 3.25")())
  show(math.tointeger("8"), string.rep("ab", 2, ","))
  show(bytes(string.upper("\xe1\xe9 abc\xc9\xff")), bytes(string.lower("\xc1\xc9 ABC")))
  show((string.gsub("\xe1\xe9\xed abc \xd3\xfa", "%a", "x")))
  show((string.gsub("\xe1\xc9\xed\xd3", "%l", "l")), (string.gsub("\xe1\xc9\xed\xd3", "%u", "u")))
  show((string.gsub("a1 \xa0\xb2\xbf.", "%W", "_")), (string.gsub("\t\v\xa0x", "%s", "s")))
  show(string.find("caf\xe9 au lait", "%a+", 4), string.match("\xc9cole", "^%u%l+$"))
  show("alo" < "\xe1lo", "\xe1lo" < "amo", "a" < "B", "B" < "a", "Z" < "a")
  show("a\0b" < "a\0c", "a\0" < "a", "abc" <= "abc", "\xe9" < "f")
  show(os.date("!%a %A %b %B %p", 86400 * 40 + 3600 * 15))
  show(os.date("!%c|%x|%X|%r", 86400 * 40 + 3600 * 15 + 61))
  show(os.date("!%Ec|%Ex|%EX|%Ey|%Od|%OH", 86400 * 40))
  local f = io.tmpfile()
  f:write("3,5 3.5 0x10 1e1\n")
  f:seek("set")
  show(f:read("n", "n", "n", "n"))
  f:seek("set")
  f:write(2.5, " ", 7, "\n")
  f:seek("set")
  show(f:read("l"))
  f:close()
end

print("C")
effects()

local candidates = {
  "pt_BR.ISO-8859-1", "pt_BR.iso88591", "pt_BR.UTF-8", "pt_BR.utf8",
  "de_DE.ISO-8859-1", "tr_TR.ISO-8859-9", "C.UTF-8",
}
for _, name in ipairs(candidates) do
  local got = os.setlocale(name)
  if got then
    print(name, got)
    effects()
    -- Categories apart: only numbers follow pt_BR here.
    os.setlocale("C")
    show(os.setlocale(name, "numeric"), os.setlocale(nil, "ctype"), os.setlocale(nil, "numeric"), os.setlocale())
    show(tostring(1.5), string.upper("\xe1"), "\xe1" < "b")
    os.setlocale("C")
  end
end

-- Names: queries, composite names, invalid ones.
show(os.setlocale(), os.setlocale(nil, "all"), os.setlocale("POSIX"), os.setlocale("C\0x"))
show(os.setlocale("no_SUCH.locale"), os.setlocale("no_SUCH", "time"), os.setlocale())
show(pcall(os.setlocale, "C", "messages"), pcall(os.setlocale, "C", "nope"))
os.setlocale("C")
