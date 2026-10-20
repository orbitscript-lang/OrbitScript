-- Stands in for Luau's bench_support.lua: runs each test once and prints
-- only its name, so every interpreter's output is the same (the testkit
-- times the whole process).
local bench = {}
function bench.runCode(f, description)
  f()
  print(description)
end
function bench.runScriptCodeUnderActor(_, f, description)
  bench.runCode(f, description)
  return false
end
return bench
