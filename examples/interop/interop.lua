-- interop: Lua code using an OrbitScript module (shapes.orb), which in turn uses a Lua
-- module (geometry.lua). Nothing is wrapped: OrbitScript enums and structs are tables
-- with metatables, and their methods are called with `:` from Lua.
--
-- Usage: orbit interop.lua [rounds]   (run it from this directory)

local shapes = require("shapes")
local Shape = shapes.Shape

local list = {
  Shape.Circle.new({ r = 1 }),
  Shape.Rect.new({ w = 3, h = 4 }),
  shapes.triangle(0, 0, 4, 0, 0, 3),
}

local rounds = tonumber(arg and arg[1]) or 1
local total
for _ = 1, rounds do
  total = shapes.total_area(list)
end

for _, s in ipairs(list) do
  print(string.format("%-20s area %7.3f  perimeter %7.3f", tostring(s), s:area(), s:perimeter()))
end
print(string.format("total area %.3f", total))
print("is a Rect:", getmetatable(list[2]) == Shape.Rect, getmetatable(list[1]) == Shape.Rect)
print("variant name:", getmetatable(list[3]).__name)
print("Lua sees only the pub items:", shapes.triangle ~= nil, shapes.geometry == nil)
