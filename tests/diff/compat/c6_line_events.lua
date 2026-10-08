-- Line events for operands split across lines: a global, upvalue or field
-- read belongs to the operator's line. The main chunk ends on its last
-- statement's line.
local s = [[
     local b = {10}
     a = b[1] X + Y b[1]
     b = 4
  ]]
for _, i in ipairs{1, 2, 128, 1000} do
  for _, j in ipairs{1, 128, 1000} do
    local subs = {X = string.rep("\n", i), Y = string.rep("\n", j)}
    local src = string.gsub(s, "[XY]", subs)
    local l = {}
    debug.sethook(function(e, line) l[#l+1] = line end, "l"); load(src)(); debug.sethook()
    print(i, j, table.concat(l, " "))
  end
end
local cases = {
  "local b = {1}\nlocal x = b[1]\n+\nb[1]",
  "local b = {f=1, 1}\nlocal x = b.f\n+\nb[1]",
  "local x = g\n+\ng",
  "local b = {'a'}\nlocal x = b[1]\n..\nb[1]",
  "local b = {1}\nlocal x = b[1]\n&\nb[1]",
  "local b = {1}\nlocal x = -b[1]\n+\nb[1]",
  "local b = {1}\nlocal x = b[1]\n+\n2",
  "local b = {1}\nlocal x = b[1]\n+\nb[1]\n*\nb[1]",
  "local b = {1}\nlocal x = b[1] + b[1]\n+\nb[1]",
  "local b = {1}\nlocal x = #b\n+\n#b",
  "x=1\n\n\n", "x=1 -- c\n\n", "\n\n", "", "local function f()\nend\n\n",
}
for _, s in ipairs(cases) do
  local l = {}
  g = 1
  debug.sethook(function(e, line) l[#l+1] = line end, "l"); load(s)(); debug.sethook()
  print((s:gsub("\n", "|")), table.concat(l, " "))
end
