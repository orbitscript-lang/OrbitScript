-- An uncaught error still closes the state.
_G.x = setmetatable({}, {__gc = function () print("finalized after the error") end})
setmetatable({}, {__gc = function () error("ignored") end})
error("the error")
