-- Lua 5.5 additions: global declarations, collective attributes,
-- read-only loop variables, table.create, utf8.offset end positions.
global print, load, pcall, utf8, table, tostring
global <const> X = 10
print(X)
local <const> a, b = 1, 2
print(a + b)
print(load("global <const> Y = 1; Y = 2"))
print(load("for i = 1, 3 do i = 5 end"))
print(load("local x <const> = 1; x = 2"))
print(load("global none; zzz = 1"))
print(utf8.offset("aλb", 2), utf8.offset("aλb", 3))
print(#table.create(10), utf8.char(955, 72), utf8.len("aλb"), utf8.codepoint("λ"))
for p, c in utf8.codes("hé") do print(p, c) end
