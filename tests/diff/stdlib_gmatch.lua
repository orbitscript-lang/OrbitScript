-- string.gmatch: captures, empty matches, init, a literal leading '^',
-- errors raised while iterating, and the exhausted iterator.
local function all(...) local t = {} for a, b in string.gmatch(...) do t[#t+1] = tostring(a) .. (b and ("/" .. tostring(b)) or "") end return table.concat(t, ",") end
print(all("hello world from lua", "%a+"))
print(all("k=v, a=b", "(%w+)=(%w+)"))
print(all("abc", ""))
print(all("abc", "^a"))
print(all("^a^a", "^a"))
print(all("abc", ".", 2))
print(all("abc", ".", -1))
print(all("abc", ".", 10))
print(all("abc", ".", 4))
print(all("abc", ".", -10))
print(all("aaa", "a*"))
print(all("ab12", "()%d"))
print(pcall(all, "ab", "(a"))
print(pcall(all, "ab", "%"))
print(pcall(string.gmatch, "ab"))
print(pcall(string.gmatch, {}, "a"))
local it = string.gmatch("a1b2", "%a(%d)") print(type(it), it(), it(), it(), it())
print(pcall(string.gmatch, "ab", "a", "x"))
do local t = {} for k in ("x y"):gmatch("%S") do t[#t + 1] = k end print(table.concat(t, ";")) end

-- A start past the end matches nothing, not even an empty string.
for _, init in ipairs{3, 4, 5, 10} do
  local n = 0
  for _ in string.gmatch("abc", "%w*", init) do n = n + 1 end
  print("gmatch init", init, n)
end
