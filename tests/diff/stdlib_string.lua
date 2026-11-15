local function p(...) print(pcall(...)) end
p(string.find, "hello world", "o w")
p(string.find, "hello world", "(o)(r)")
p(string.find, "hello", "l", -2)
p(string.find, "a.b", ".", 1, true)
p(string.match, "key = value", "(%w+)%s*=%s*(%w+)")
p(string.match, "  x", "()x")
p(string.match, "hello", ".-(l+)(.*)")
p(string.gsub, "hello world", "(o)", "[%1%0]")
p(string.gsub, "abc", "", "-")
p(string.gsub, "abc", "b*", "-")
p(string.gsub, "aaa", "^a", "b")
p(string.gsub, "abc", "%w", "%0%0", 2)
p(string.gsub, "abc", "%w", {a=1, b=false})
p(string.gsub, "abc", "%w", function(c) if c == "b" then return nil end return c .. "!" end)
p(string.gsub, "hello world", "%w+", function(w) return string.upper(w) end)
p(string.gsub, "abc", "(b)", {b=true})
p(string.gsub, "abc", ".", "%2")
p(string.gsub, "abc", ".", "%")
p(string.gsub, "", "", "x")
p(string.gsub, "x = 1, y = 2", "(%w+) = (%w+)", "%2 = %1")
p(string.gsub, "abc", "^", "x")
p(string.gsub, "a b c", " ", "", -1)
p(string.find, "THE (quick) fox", "%((%a+)%)")
p(string.find, "[[]] [][] [[[[", "%f[[].")
p(string.gsub, "aaa aa a aaa a", "%f[%w]a", "x")
p(string.rep, "ab", 3, ",")
-- Nothing to repeat: empty at once, however many times.
p(string.rep, "", math.maxinteger)
p(string.rep, "", math.maxinteger, "")
p(string.rep, "", 3, ",")
p(string.rep, "x", math.maxinteger, "")
p(string.reverse, "abc") p(string.upper, "aBc1") p(string.lower, "ABc")
p(string.format, "%5.2f|%e|%g|%g|%g|%g|%#g|%.0e|%#.0f|%G", 3.14159, 12345.678, 1e20, 1e-5, 123456789, 0.0001, 1.0, 5.5, 2.0, 1e-10)
p(string.format, "%.3f %.20g %g %g %.14g", 2.5, 0.1, 100000, 1e6, 2^53)
p(string.format, "%q", "\r\n\t\1\0012\127\255x\"\\")
p(string.format, "%s|%10.4s|", 12.0, 1e100)
p(string.format, "%i|%5.1f|%-8.3e|% d|%+d", 3.0, -0.05, 1/3, 5, 5)
p(string.format, "%#12o|%#10x|%#-17X|%013i|%2.5d|%.u|%-16c|", 10, 100, 100, -100, -100, 0, 97)
p(string.format, "%a %A %.3a %a %a %.0a %.1a %#a", 1.0, 255.5, 1/3, 0.0, -0.0, 1.5, 1.96875, 1.0)
p(string.format, "%q %q %q %q %q %q", 0.1, math.mininteger, 1/0, 0/0, -1/0, 1e100)
p(string.format, "%5.1s|%c|%x|%X|%o", "abc", 65, 255, 255, 8)
p(string.format, "%100.3d", 1) p(string.format, "%10q", 1) p(string.format, "%5%", 1) p(string.format, "%d %d", 1)
p(string.format, "%10s", "a\0") p(string.format, "%d", 1.5) p(string.format, "%y")
p(string.format, "%.99f", 1/3)
p(string.format, "%5.3s|%-5s|%05d|%+05d|% 05.1f|%-+8.2f|", "abcdef", "ab", 42, 42, 3.14159, 2.5)
p(string.format, "%g %g %g %g %g", 0/0, -(0/0), 1/0, -1/0, -0.0)
p(string.format, "%.3g %.10g %#.3g %g %g", 1234567, 1/3, 1, 1e15, 123456)

-- Error order: integer and e/f/g conversions check the argument before the
-- spec, %a and %c/%s/%p/%q check the spec first; an unfinished capture is
-- an error only where a result uses it.
do
for _, c in ipairs{"d", "i", "u", "o", "x", "X", "c", "a", "e", "f", "g", "s", "p", "q"} do
  local function t(spec, v) local ok, e = pcall(string.format, spec, v) print(spec, tostring(v), ok, e) end
  t("%#!" .. c, {})
  t("%#" .. c, {})
  t("%.3" .. c, {})
  t("%5.3" .. c, nil)
end
end
do
for _, spec in ipairs{"%123d", "%.123d", "%.99d", "%99.99d", "%-+ #0d", "%--5d", "%000005d", "%.180A", "%1234s", "%#.123c"} do
  print(spec, pcall(string.format, spec, {}))
end
end
do
for _, c in ipairs{"a", "A", "e", "E", "f", "g", "G"} do print(c, pcall(string.format, "%.180" .. c, {})) print(c, pcall(string.format, "%#!" .. c, {})) end
print(pcall(string.find, "ab", "(()a"))
print(pcall(string.find, "ab", "(a"))
print(pcall(string.match, "ab", "(a"))
print(pcall(string.gsub, "ab", "(a", "x"))
print(pcall(string.gsub, "ab", "(a", "%0"))
print(pcall(string.gsub, "ab", "(a", "%1"))
print(pcall(string.gsub, "ab", "(a", {}))
print(pcall(string.gsub, "ab", "(a", print))
print(pcall(string.gsub, "ab", "(a)(b", "%1"))
print(pcall(string.gsub, "ab", "(a)(b", "%2"))
print(pcall(string.find, "ab", "(a)(b", 1, true))
print(pcall(string.find, "ab", "a(b"))
end

-- gsub returns the subject itself when nothing was replaced.
do
  local s = string.rep("a", 100)
  local same = function(r) return string.format("%p", s) == string.format("%p", r) end
  print("gsub reuse", same((string.gsub(s, "b", "c"))), same((string.gsub(s, ".", {x = "y"}))),
        same((string.gsub(s, ".", function() return nil end))),
        same((string.gsub(s, ".", function(x) return x end))), same((string.gsub(s, "a", "%0"))))
end

-- A million function replacements (and through `__index`): no stack overflow.
do
  local s = string.rep("a", 1000000)
  local r, n = string.gsub(s, "a", function(c) return c end)
  print(#r, n, r == s)
  r, n = string.gsub(s, "a", setmetatable({}, {__index = function(_, c) return c .. "b" end}))
  print(#r, n, r == string.rep("ab", 1000000))
  r, n = string.gsub(string.rep("abc ", 200000), "%w+", function(w) if #w % 2 == 1 then return w:upper() end end)
  print(#r, n, r:sub(1, 12), r:sub(-12))
end

-- More results than the stack holds: an error, not ten million values.
do
  print(pcall(function() return select("#", ("x"):rep(1e7):byte(1, -1)) end))
  print(pcall(function() return select("#", string.unpack(("b"):rep(1e6), ("x"):rep(1e6))) end))
  print(select("#", ("x"):rep(500000):byte(1, -1)))
  print(select("#", string.unpack(("b"):rep(500000), ("x"):rep(500000))))
end

-- Argument checks, in argument order, and results built from the subject.
do
  print(pcall(string.len))
  print(pcall(string.sub))
  print(pcall(string.sub, "abc", "x"))
  print(pcall(string.rep))
  print(pcall(string.rep, "x", 3, {}))
  print(string.rep("ab", 3, ","), string.rep("", 1e10), string.rep("x", -1), string.rep("x", 0, ","))
  print(pcall(string.upper))
  print(pcall(string.char, 256))
  print(pcall(string.char, "x"))
  print(string.char(), string.char(72, 105))
  print(select("#", string.byte("abc", 1, -1)), select("#", string.byte("")), string.byte("abc", -1))
  print(string.len(12.5), string.sub(12345, 2, -2), string.upper(1e100), string.reverse(123))
  print(pcall(string.find, "abc", "b", "x"))
  print(string.find("abc", "b", 1, true), string.find("abc", "(b)()"), string.match("abc", "()b"))
  print(string.find("abc", "b", 10), string.match("abc", ".", -1), string.find("a.c", ".", 1, 1))
  print(pcall(string.find, "a", "%"))
  print(pcall(string.match, "a", "(()"))
  print(pcall(string.gmatch, "a"))
  for k, v in string.gmatch("a=1, b=2", "(%w+)=(%w+)") do io.write(k, v, ";") end print()
  for w in string.gmatch("abc", "", 2) do io.write("[", w, "]") end print()
  for a, b in string.gmatch(123123, "(2)()") do io.write(a, b, ";") end print()
  local it = string.gmatch("a", "%")
  print(pcall(it))
end

-- pack, unpack and packsize: values are checked as each option needs one.
do
  print(pcall(string.pack, "i4"))
  print(pcall(string.pack, "i4", "x"))
  print(pcall(string.pack, "z", {}))
  print(pcall(string.pack, "c2", "abc"))
  print(pcall(string.pack, "i1", 200))
  print(pcall(string.pack, "Xz"))
  print(pcall(string.pack, "!3i4", 1))
  print(pcall(string.unpack, "i4", "ab"))
  print(pcall(string.unpack, "i4", "abcd", 10))
  print(pcall(string.unpack, "z", "abc"))
  print(pcall(string.packsize, "s"))
  print(pcall(string.packsize, "q"))
  print(string.packsize("i4i8"), string.packsize("!8i2d"))
  print(string.unpack("z B s1 c3 >i2 d", string.pack("z B s1 c3 >i2 d", "hi", 7, "yo", "abc", -2, 1.5)))
  print(string.unpack("i16", string.pack("i16", -3)))
  print(pcall(string.unpack, "i9", string.rep("\xff", 8) .. "\x01"))
end
