-- geometry: a plain Lua module that shapes.orb uses with `use geometry;`.

local M = {}

M.TAU = 2 * math.pi

local Point = {}
Point.__index = Point

function M.point(x, y)
  return setmetatable({ x = x, y = y }, Point)
end

-- A Lua method: OrbitScript calls it as `p.distance(q)`.
function Point:distance(q)
  return math.sqrt((self.x - q.x) ^ 2 + (self.y - q.y) ^ 2)
end

function Point:__tostring()
  return string.format("(%g, %g)", self.x, self.y)
end

-- A plain function: OrbitScript calls it as `geometry::polygon_area(points)`.
function M.polygon_area(points)
  local sum = 0
  for i = 1, #points do
    local p, q = points[i], points[i % #points + 1]
    sum = sum + p.x * q.y - q.x * p.y
  end
  return math.abs(sum) / 2
end

return M
