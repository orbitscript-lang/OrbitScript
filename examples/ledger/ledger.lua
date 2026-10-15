-- ledger: the same program as ledger.orb, in plain Lua 5.5, for comparison.
-- It must print exactly what ledger.orb prints (examples/README.md says how that is checked).
--
-- Usage: lua ledger.lua [rounds]

local script = [[
# A tiny bank. Amounts may be integers or decimals.
open alice 100
open bob 20
open carol 5
deposit carol 15.5
transfer alice bob 30
transfer bob alice 500
transfer alice dave 5
open bob 1
withdraw carol 2.25

# A batch commits all of its commands or none of them.
batch
  transfer alice carol 10
  transfer carol bob 100
end
batch
  transfer alice carol 10
  deposit bob 5
  batch
    withdraw bob 1
    explode now
  end
end
deposit alice ten
report
]]

local Tx = {}
Tx.__index = Tx

function Tx.begin(ledger, label)
  local saved = {}
  for name, balance in pairs(ledger.balances) do saved[name] = balance end
  return setmetatable({ ledger = ledger, saved = saved, label = label, done = false }, Tx)
end

function Tx:commit() self.done = true end

function Tx:__close(err)
  if self.done then return end
  self.ledger.balances = self.saved
  if self.label ~= nil then
    local why = err ~= nil and " after error: " .. tostring(err) or ""
    self.ledger:say("rolled back " .. self.label .. why)
  end
end

local function amount(text)
  local n = tonumber(text)
  if n == nil or n <= 0 then return nil, "bad amount '" .. tostring(text) .. "'" end
  return n
end

local Ledger = {}
Ledger.__index = Ledger

function Ledger.new(verbose)
  return setmetatable({ balances = {}, verbose = verbose, depth = 0 }, Ledger)
end

function Ledger:say(text)
  if self.verbose then print(string.rep("  ", self.depth) .. text) end
end

function Ledger:account(name)
  if self.balances[name] == nil then return nil, "no account '" .. tostring(name) .. "'" end
  return name
end

function Ledger:take(name, n)
  local err
  name, err = self:account(name)
  if name == nil then return nil, err end
  self.balances[name] = self.balances[name] - n
  if self.balances[name] < 0 then return nil, name .. " would go negative" end
  return true
end

function Ledger:apply(words)
  local cmd = words[1]
  if cmd == "open" then
    local name = words[2]
    if self.balances[name] ~= nil then return nil, "account '" .. name .. "' already exists" end
    local n, err = amount(words[3])
    if n == nil then return nil, err end
    self.balances[name] = n
  elseif cmd == "deposit" then
    local name, err = self:account(words[2])
    if name == nil then return nil, err end
    local n
    n, err = amount(words[3])
    if n == nil then return nil, err end
    self.balances[name] = self.balances[name] + n
  elseif cmd == "withdraw" then
    local n, err = amount(words[3])
    if n == nil then return nil, err end
    local ok
    ok, err = self:take(words[2], n)
    if not ok then return nil, err end
  elseif cmd == "transfer" then
    local n, err = amount(words[4])
    if n == nil then return nil, err end
    local ok
    ok, err = self:take(words[2], n)
    if not ok then return nil, err end
    local to
    to, err = self:account(words[3])
    if to == nil then return nil, err end
    self.balances[to] = self.balances[to] + n
  elseif cmd == "report" then
    self:report()
  elseif cmd == "explode" then
    error("boom (" .. words[2] .. ")", 0)
  else
    return nil, "unknown command '" .. cmd .. "'"
  end
  return true
end

function Ledger:run(cmd)
  if cmd.tag == "simple" then
    self:say(cmd.line)
    local tx <close> = Tx.begin(self, nil)
    local ok, err = self:apply(cmd.words)
    if not ok then return nil, err end
    tx:commit()
  else
    self:say("batch {")
    self.depth = self.depth + 1
    local _ <close> = setmetatable({}, { __close = function()
      self.depth = self.depth - 1
      self:say("}")
    end })
    local tx <close> = Tx.begin(self, "batch")
    for _, c in ipairs(cmd.body) do
      local ok, err = self:run(c)
      if not ok then return nil, err end
    end
    tx:commit()
  end
  return true
end

function Ledger:report()
  local names = {}
  for name in pairs(self.balances) do names[#names + 1] = name end
  table.sort(names)
  local total = 0
  for _, name in ipairs(names) do
    local balance = self.balances[name]
    total = total + balance
    self:say(string.format("  %-8s%9.2f", name, balance))
  end
  self:say(string.format("  %-8s%9.2f", "total", total))
end

local function parse(text)
  local top = {}
  local open_blocks = { top }
  for line in text:gmatch("[^\n]+") do
    local words = {}
    for w in line:gmatch("%S+") do words[#words + 1] = w end
    if #words > 0 and words[1]:sub(1, 1) ~= "#" then
      local block = open_blocks[#open_blocks]
      if words[1] == "batch" then
        local body = {}
        block[#block + 1] = { tag = "batch", body = body }
        open_blocks[#open_blocks + 1] = body
      elseif words[1] == "end" then
        open_blocks[#open_blocks] = nil
      else
        block[#block + 1] = { tag = "simple", line = line:match("^%s*(.-)%s*$"), words = words }
      end
    end
  end
  return top
end

local commands = parse(script)
local rounds = tonumber(arg and arg[1]) or 1
for round = 1, rounds do
  local ledger = Ledger.new(round == rounds)
  for _, cmd in ipairs(commands) do
    local ok, result, err = pcall(ledger.run, ledger, cmd)
    if not ok then
      ledger:say("error: " .. tostring(result))
    elseif result == nil then
      ledger:say("error: " .. err)
    end
  end
end
