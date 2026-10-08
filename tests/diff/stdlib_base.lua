-- assert: the default message carries the caller's position, given messages don't.
print(pcall(assert, false))
print(pcall(assert, false, "msg"))
print(pcall(assert, nil))
print(pcall(function() assert(false) end))
print(pcall(function() assert(1 == 2, nil) end))
print(pcall(function() assert(false, {}) end))
print(pcall(function() local f = assert; f(false) end))
