-- Line hooks in a stripped function fire on entry and on backward jumps,
-- with no line; a dumped function keeps lastlinedefined.
local function foo ()
  local a = 1
  for i = 1, 3 do a = a + i end
  return a
end
local s = load(string.dump(foo, true))
local ev = {}
debug.sethook(function (e, l) ev[#ev+1] = e .. ":" .. tostring(l) end, "l")
s(); s()
debug.sethook(nil)
print(table.concat(ev, " "))
local i = debug.getinfo(s, "S")
print(i.linedefined, i.lastlinedefined, i.short_src, i.what)
