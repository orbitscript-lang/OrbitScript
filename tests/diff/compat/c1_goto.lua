-- goto and labels: loops, continue, backward jumps, fresh bindings, and compile errors.
local i, acc = 1, ""
::top::
if i <= 3 then acc = acc .. i .. " " i = i + 1 goto top end
print(acc)
acc = ""
for j = 1, 5 do
  if j % 2 == 0 then goto continue end
  acc = acc .. j .. " "
  ::continue::
end
print(acc)
local fs = {}
do
  local k = 1
  ::again::
  local v = k * 10
  fs[k] = function() return v end
  k = k + 1
  if k <= 3 then goto again end
end
print(fs[1](), fs[2](), fs[3]())
for a = 1, 3 do
  for b = 1, 3 do
    if a * b == 4 then goto out end
  end
end
::out::
print("left nested loops")
local n = 0
while true do
  n = n + 1
  if n > 4 then goto done end
end
::done::
print(n)
repeat
  local r = 1
  goto skip
  ::skip::
until r == 1
do goto e1; local x; ::e1:: end
print("label at block end")
local function c(src)
  local f, e = load(src, "=g")
  print(f and "ok" or e)
end
c("goto l1; local a; ::l1:: print(a)")
c("do\ngoto l\nlocal x\n::l::\nprint(x)\n\nend\n")
c("repeat goto cont; local x; ::cont:: until x")
c("goto nowhere")
c("do ::a:: end goto a")
c("::a:: ::a::")
c("::a:: do ::a:: end")
c("do ::a:: end ::a::")
c("local function f() goto a end ::a::")
c("goto f; local x; ::f:: return")
c("goto f; global y; ::f:: y = 1")
c("goto f; local x; ::f:: ; ::g:: ;")
c("while true do goto continue; local y = 1; ::continue:: end")
