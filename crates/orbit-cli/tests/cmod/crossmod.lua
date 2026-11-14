-- crossmod.c's calls go to another copy of the runtime (see there).
local m = require("crossmod")
assert(m.answer == 42)
assert(m.add(2, 3) == 5)
-- A coroutine's state too.
assert(coroutine.wrap(function() return m.add(10, 20) end)() == 30)
-- An error raised in the other copy unwinds to this one's protected call.
local ok, err = pcall(m.fail)
assert(not ok and err == "failed in the other copy (7)", err)
local ok2, err2 = pcall(m.add, 1, "x")
assert(not ok2 and err2:find("number expected"), err2)
print("ok")
