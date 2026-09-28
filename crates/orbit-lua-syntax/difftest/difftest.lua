-- The reference side of the differential tests; see README.md. Run with lua5.5:
--   difftest.lua messages CORPUS      per chunk: OK or the message load(chunk, "=t") gives
--   difftest.lua ctc CORPUS NAME      per chunk: whether local NAME is a compile-time constant
--   difftest.lua compare DIR OUT NAME...
--                                     check that OUT/NAME.{plain,renamed}.lua compile to the
--                                     same stripped bytecode as DIR/NAME.lua

local function corpus(path)
  local f = assert(io.open(path, "rb"))
  local data = f:read("a")
  f:close()
  local chunks, pos = {}, 1
  while pos + 3 <= #data do
    local len
    len, pos = string.unpack("<I4", data, pos)
    chunks[#chunks + 1] = data:sub(pos, pos + len - 1)
    pos = pos + len
  end
  return chunks
end

local mode = arg[1]
if mode == "messages" then
  for _, chunk in ipairs(corpus(arg[2])) do
    local line
    if chunk:byte(1) == 27 then
      line = "binary chunk"
    else
      local fn, err = load(chunk, "=t", "t")
      -- "C stack overflow" is a run-time error, so a message handler may add a traceback.
      line = fn and "OK" or err:gsub("\nstack traceback:.*", "")
    end
    io.write((line:gsub("\n", "\\n")), "\n")
  end
elseif mode == "ctc" then
  -- A local that lives in a register appears by name in the debug information.
  for _, chunk in ipairs(corpus(arg[2])) do
    local fn = load(chunk, "=t")
    if not fn then print("ERR")
    else print(string.dump(fn):find(arg[3], 1, true) and "reg" or "ctc")
    end
  end
elseif mode == "compare" then
  local bad = 0
  for i = 4, #arg do
    local name = arg[i]
    local original = string.dump(assert(loadfile(arg[2] .. "/" .. name .. ".lua")), true)
    for _, kind in ipairs{"plain", "renamed"} do
      local f, err = loadfile(arg[3] .. "/" .. name .. "." .. kind .. ".lua")
      if not f then
        print(name, kind, err); bad = bad + 1
      elseif string.dump(f, true) ~= original then
        print(name, kind, "bytecode differs"); bad = bad + 1
      end
    end
  end
  print(bad == 0 and "all identical" or (bad .. " mismatches"))
  os.exit(bad == 0)
else
  error("unknown mode " .. tostring(mode))
end
