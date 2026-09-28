-- Integer array permutations: array reads, writes and swaps.
local function fannkuch(n)
  local p, q, s, sign, maxflips, sum = {}, {}, {}, 1, 0, 0
  for i = 1, n do p[i] = i; q[i] = i; s[i] = i end
  repeat
    -- Copy and flip.
    local q1 = p[1]
    if q1 ~= 1 then
      for i = 2, n do q[i] = p[i] end
      local flips = 1
      while true do
        local qq = q[q1]
        if qq == 1 then
          sum = sum + sign * flips
          if flips > maxflips then maxflips = flips end
          break
        end
        q[q1] = q1
        if q1 >= 4 then
          local i, j = 2, q1 - 1
          repeat q[i], q[j] = q[j], q[i]; i = i + 1; j = j - 1; until i >= j
        end
        q1 = qq; flips = flips + 1
      end
    end
    -- Permute.
    if sign == 1 then
      p[2], p[1] = p[1], p[2]; sign = -1
    else
      p[2], p[3] = p[3], p[2]; sign = 1
      for i = 3, n do
        local sx = s[i]
        if sx ~= 1 then s[i] = sx - 1; break end
        if i == n then return sum, maxflips end
        s[i] = i
        local t = p[1]; for j = 1, i do p[j] = p[j + 1] end; p[i + 1] = t
      end
    end
  until false
end

local n = tonumber(arg[1]) or 10
local sum, flips = fannkuch(n)
print(sum, "\nPfannkuchen(" .. n .. ") = " .. flips)
