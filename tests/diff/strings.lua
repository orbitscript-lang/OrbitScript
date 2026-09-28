-- String library: patterns, formatting, byte handling.
local s = "The quick brown fox jumps over the lazy dog"
print(#s, s:upper(), s:lower(), s:reverse(), s:sub(5, 9), s:sub(-3), s:sub(0), s:sub(10, 5))
print(s:find("quick"), s:find("o", 1, true), s:find("(%a+) (%a+)$"), s:match("(%a+)$"))
print(s:gsub("o", "0"), s:gsub("%w+", string.upper, 3), s:gsub("(%w+) (%w+)", "%2 %1"))
for w in s:gmatch("%a%a%a%a%a") do io.write(w, ",") end print()
print(("x"):rep(3, "-"), ("abc"):byte(1, -1), string.char(72, 105))
print(("%5s|%-5s|%05d|%+.3e"):format("ab", "cd", 42, 12345.678))
print(("key = value"):match("^(%w+)%s*=%s*(%w+)$"))
print(("[[nested]]"):find("%[%[(.-)%]%]"), ("f(a(b)c)"):match("%b()"), ("THE (quick) fox"):find("%f[%a]%a+"))
print(pcall(string.rep), pcall(string.format, "%d", 1.5))
print(tostring(nil), tostring(true), ("x"):len(), #"\0a\0", "\u{48}\65\x42\z
      continued")
print(string.format("%q", "a\nb\0c\"\\"), string.format("%q", math.mininteger))
