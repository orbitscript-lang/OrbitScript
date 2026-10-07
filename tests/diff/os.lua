-- The os library: dates in UTC, time normalization, errors, environment,
-- files and processes. Local-time output is not compared (it depends on
-- the machine's zone), only its shape.

local function fields(t)
  local keys = {"day", "hour", "isdst", "min", "month", "sec", "wday", "yday", "year"}
  local n = 0
  for _ in pairs(t) do n = n + 1 end
  local out = {n}
  for i, k in ipairs(keys) do out[i + 1] = k .. "=" .. tostring(t[k]) end
  return table.concat(out, " ")
end

local function show(...)
  local t = {...}
  for i = 1, select("#", ...) do t[i] = tostring(t[i]) end
  print(select("#", ...), table.concat(t, " | "))
end

-- Formats.
print(os.date("!%c", 0))
local t = 86400 * 45 + 3600 * 15 + 61
print(os.date("!%x %X %p %y %j %U %W %w %a %A %b %B %h %e %D %F %T %R", t))
print(os.date("!%n%t%% %G %g %V %u %C %I %M %S %z %Z", t))
print(os.date("!%Ec|%EC|%Ex|%EX|%Ey|%EY|%Od|%Oe|%OH|%OI|%Om|%OM|%OS|%Ou|%OU|%OV|%Ow|%OW|%Oy", 0))
print(os.date("!%c", -2^40), os.date("!%C %y %G %g", -2^40))
print(os.date("!%U %W %V %G", 1104537600))
for _, y in ipairs{1999, 2000, 2004, 2015, 2020, 2021, 2100} do
  for _, d in ipairs{1, 2, 3, 4, 7, 360, 363, 365, 366} do
    local s = os.time{year = y, month = 1, day = d, hour = 0}
    local line = os.date("!%Y-%m-%d %a %j %U %W %V %G %g %u %w", s)
    print(line)
  end
end
print(os.date("") == "", os.date("!") == "", os.date("!\0\0") == "\0\0")
print(#os.date(string.rep("%", 200)))
print(fields(os.date("!*t", 0)))
print(fields(os.date("!*t", 951782400)))

-- os.time and normalization (UTC here: the harness machines run in UTC).
print(os.time{year = 2000, month = 1, day = 1, hour = 0})
print(os.time{year = 2000, month = 14, day = -3, hour = 25, min = 61, sec = -1})
local d = {year = 2005, month = 1, day = 1, hour = 1, min = 0, sec = -3602}
print(os.time(d), fields(d))
print(os.time{year = 1000, month = 1, day = 1, hour = "2"})
print(os.time{year = 2147483648 + 1899, month = 12, day = 31, hour = 23, min = 59, sec = 59})
print(math.type(os.time()), math.type(os.clock()))

-- Errors.
show(pcall(os.date, "%Q"))
show(pcall(os.date, "ab%Qxy"))
show(pcall(os.date, "%"))
show(pcall(os.date, "%Ea"))
show(pcall(os.date, "%E"))
show(pcall(os.date, "*t", 1.5))
show(pcall(os.date, "%Y", "x"))
show(pcall(os.date, "%Y", 2^60))
show(pcall(os.time, 3))
show(pcall(os.time, {year = 2000}))
show(pcall(os.time, {hour = 12}))
show(pcall(os.time, {year = 1000, month = 1, day = 1, hour = "x"}))
show(pcall(os.time, {year = 1000, month = 1, day = 1, hour = 1.5}))
show(pcall(os.time, {year = 1000, month = 2^40, day = 1}))
show(pcall(os.time, {year = -2147483648 + 1899, month = 1, day = 1}))
show(pcall(os.time, {year = 2147483648 + 1900, month = 1, day = 1}))
show(pcall(os.time, {year = 0, month = 1, day = 2^32}))
show(pcall(os.time, {year = 2147483648 + 1899, month = 12, day = 31, hour = 23, min = 59, sec = 60}))
show(pcall(os.time, {year = -math.maxinteger, month = 1, day = 1}))
show(os.difftime(10, 3), math.type(os.difftime(10, 3)), os.difftime(3.0, 1))
show(pcall(os.difftime, 1.5, 1))
show(pcall(os.difftime))

-- Locale.
show(os.setlocale(), os.setlocale("C"), os.setlocale(""), os.setlocale("POSIX"))
show(os.setlocale("xx_XX"), os.setlocale(nil, "numeric"), os.setlocale("C", "all"))
show(pcall(os.setlocale, "C", "bad"))

-- Environment.
show(type(os.getenv("PATH")), os.getenv("ORBIT_SURELY_NOT_SET_42"))
show(pcall(os.getenv))

-- Files.
local name = os.tmpname()
show(type(name), os.remove(name))
local ok, msg, code = os.remove(name)
show(ok, msg == name .. ": No such file or directory", code)
show(os.rename(name, name .. ".x"))
show(pcall(os.remove))

-- Processes.
show(os.execute())
show(os.execute("exit 3"))
show(os.execute("true"))
show(os.execute("kill -9 $$"))
