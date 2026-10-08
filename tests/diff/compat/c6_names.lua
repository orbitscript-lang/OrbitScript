-- Error messages use a table's (or full userdata's) `__name`, when it is a
-- string, in place of its type name.
XX = setmetatable({}, {__name = "My Type"})
local function t(s, ...) print(select(2, pcall(load(s), ...))) end
t"return XX + 1"
t"return XX < XX"
t"return {} < XX"
t"XX()"
t"return XX .. 'a'"
t"for i = XX, 1 do end"
t"return -XX"
t"return XX & 1"
t"local y = XX; return y + 1"
t"return math.sin(XX)"
t("return ~(...)", XX)
t("local a = ...; return string.rep(a)", XX)
t("local a = ...; return a // 1", XX)
t("local a = ...; return 1 < a", XX)
t("local a = ...; return a <= 'x'", XX)
t("local a = ...; for i = 1, a do end", XX)
t("local a = ...; for i = 1, 2, a do end", XX)
print(pcall(table.concat, {XX}))
-- A non-string `__name` is ignored.
YY = setmetatable({}, {__name = 7})
t"return YY + 1"
-- So is `__name` in the shared metatable of other types.
debug.setmetatable(nil, {__name = "Nil Type"})
t"local a; return a + 1"
t"local a; return a()"
debug.setmetatable(nil, nil)
