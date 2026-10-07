-- Debug calls on a suspended coroutine after collections: locals, upvalues,
-- tracebacks and setlocal must see only live values.
local function body(a)
  local t = {a, tostring(a)}
  local s = string.rep and ("x" .. a) or tostring(a)
  coroutine.yield(#t)
  t[3] = "after"
  return t[1], t[2], t[3], s
end

local cos = {}
for i = 1, 20 do
  local co = coroutine.create(body)
  assert(coroutine.resume(co, i))
  cos[i] = co
end

-- Garbage that the parked stacks don't reference.
for i = 1, 200 do local _ = {i, tostring(i)} end
collectgarbage()
collectgarbage()

for i = 1, 20, 7 do
  local co = cos[i]
  local n = 1
  while true do
    local name, v = debug.getlocal(co, 1, n)
    if not name then break end
    if type(v) == "table" then v = "table" end
    print(i, n, name, v)
    n = n + 1
  end
  print(debug.traceback(co))
  print(debug.getinfo(co, 1, "l").currentline, debug.getinfo(co, 0, "n").name)
  print(debug.setlocal(co, 1, 1, i * 100))
  collectgarbage()
end

for i = 1, 20 do
  local ok, a, b, c = coroutine.resume(cos[i])
  if i % 7 == 1 then print(i, ok, a, b, c) end
end
