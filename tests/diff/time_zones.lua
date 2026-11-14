-- os.date and os.time in the local time zone, which time_zones.env sets
-- (TZ): conversions at fixed instants from year 1 to 5138, and os.time on
-- awkward local times (gaps, overlaps, out-of-range fields) with isdst
-- unset, true and false.
local ts = {0, 1, -1, 86399, 951782400, 1109635200, 1110094200, 1143943200, 1162087200, 1667116800, 1679187600, 1699167600, 1711846800, 1730015999, 1730016000, 1762070400, 2147483647, 2147483648, 4102444800, 7258118400, 32503680000, 253402300799, -2208988800, -5000000000, -62135596800, 1e11}
for _, t in ipairs(ts) do
  local ok, s = pcall(os.date, "%Y-%m-%d %H:%M:%S %Z %z %j %a %U %W %G %V", t)
  local d = ok and os.date("*t", t)
  local ok2, back = pcall(os.time, d or {})
  if not ok2 then back = "ERR" end
  print(t, s, ok and tostring(d.isdst), back)
end
-- os.time normalisation and isdst handling
for _, spec in ipairs{{2024,3,10,2,30,0}, {2024,11,3,1,30,0}, {2024,3,31,1,30,0}, {2024,10,27,1,30,0}, {2024,13,35,25,61,61}, {1969,12,31,23,59,59}, {2038,1,19,3,14,8}, {2024,4,7,2,0,0}, {2024,10,6,2,0,0}} do
  for _, dst in ipairs{"nil", "true", "false"} do
    local t = {year=spec[1], month=spec[2], day=spec[3], hour=spec[4], min=spec[5], sec=spec[6]}
    if dst == "true" then t.isdst = true elseif dst == "false" then t.isdst = false end
    local ok, r = pcall(os.time, t)
    if not ok then r = "ERR" end
    print(table.concat(spec, "-"), dst, r, ok and os.date("%Y-%m-%d %H:%M:%S %Z", r), t.isdst, t.yday, t.wday)
  end
end
