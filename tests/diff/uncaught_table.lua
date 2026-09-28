-- An uncaught non-string error object.
local function f() error({code = 1}) end
f()
