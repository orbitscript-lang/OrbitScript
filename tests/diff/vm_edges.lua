-- How many arguments __close gets, nil errors, numeric strings as 'for'
-- bounds, pairs' fourth value, the 255-value return limit and deep
-- tracebacks.
local function f2c(n)
  return setmetatable({}, {__close = function(...)
    print(n, select("#", ...), select(2, ...))
  end})
end

-- No error: only the value. An error: the value and the error.
do local a <close> = f2c("scope") end
local function ret() local a <close> = f2c("return"); return 1 end
ret()
print(pcall(function() local a <close> = f2c("error"); error("E") end))
print(pcall(function() local a <close> = f2c("nil error"); error(nil) end))
for _ in function() return nil end, nil, nil, f2c("for") do end

-- coroutine.close without an error passes none; with one, the error.
local co = coroutine.create(function()
  local a <close> = f2c("co a"); local b <close> = f2c("co b"); coroutine.yield()
end)
coroutine.resume(co); print(coroutine.close(co))
co = coroutine.create(function()
  local a <close> = f2c("co err a")
  local b <close> = setmetatable({}, {__close = function() error("in close") end})
  coroutine.yield()
end)
coroutine.resume(co); print(coroutine.close(co))

-- A nil error reads as a message past the message handler.
print(pcall(error, nil))
print(xpcall(function() error(nil) end, function(m) print("handler", m) return m end))
print(coroutine.resume(coroutine.create(function() error(nil) end)))

-- Numeric strings as 'for' bounds become floats.
for i = "10", "1", "-2" do io.write(math.type(i), " ", i, "; ") end print()
for i = 1, "0x3" do io.write(math.type(i), " ", i, "; ") end print()
for i = "1.5", 3 do io.write(i, "; ") end print()
for _, src in ipairs{"for i='a',2 do end", "for i=1,'a' do end",
                     "for i=1,2,'a' do end", "for i=1,2,'0' do end",
                     "for i='a',{} do end", "for i={},1.0,0 do end"} do
  print(pcall(load(src)))
end

-- pairs returns four values; __pairs' fourth closes with the loop.
print(select("#", pairs({})))
local t = setmetatable({}, {__pairs = function()
  return function(_, i) if i < 3 then return i + 1 end end, nil, 0, f2c("pairs tbc")
end})
for i in pairs(t) do io.write(i, " ") end print()
print(select("#", pairs(setmetatable({}, {__pairs = function() return 1, 2, 3, 4, 5 end}))))

-- Up to 254 returned values; 255 is a compile error.
print(#{assert(load("return 10" .. string.rep(",10", 253)))()})
local f, msg = load("return 10" .. string.rep(",10", 254))
print(f, msg and msg:find("too many returns") ~= nil)

-- A traceback of a very deep stack is quick.
local function deep(n) if n == 0 then return #debug.traceback() end return (deep(n - 1)) end
print(deep(100000) > 0)
