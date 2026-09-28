-- Method calls through metatables, closures and upvalues.
local Point = {}
Point.__index = Point
function Point.new(x, y) return setmetatable({x = x, y = y}, Point) end
function Point:add(o) return Point.new(self.x + o.x, self.y + o.y) end
function Point:dot(o) return self.x * o.x + self.y * o.y end

local Counter = setmetatable({}, {__index = Point})
Counter.__index = Counter
function Counter.new() local c = Point.new(0, 0); return setmetatable(c, Counter) end
function Counter:bump(d) self.x = self.x + d; return self end

local function make_adder(k) return function (v) return v + k end end

local n = tonumber(arg[1]) or 2000000
local acc = Point.new(0, 0)
local step = Point.new(1, 2)
local c = Counter.new()
local add3 = make_adder(3)
local total = 0
for i = 1, n do
  acc = acc:add(step)
  c:bump(1)
  total = total + add3(i % 10) + c:dot(step)
end
print(acc.x, acc.y, c.x, total)
