-- An uncaught error object with __tostring is reported through it.
print("before")
error(setmetatable({}, {__tostring = function () return "custom error object" end}))
