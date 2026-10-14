-- calc: the same calculator as calc.orb, in plain Lua 5.5, for comparison.
-- It must print exactly what calc.orb prints (examples/README.md says how that is checked).
--
-- Usage: lua calc.lua [rounds]

local function describe(t)
  if t.tag == "num" then return "number " .. tostring(t.value)
  elseif t.tag == "ident" then return "name '" .. t.name .. "'"
  elseif t.tag == "op" then return "'" .. t.op .. "'"
  elseif t.tag == "open" then return "'('"
  elseif t.tag == "close" then return "')'"
  elseif t.tag == "comma" then return "','"
  elseif t.tag == "assign" then return "'='"
  elseif t.tag == "end" then return "end of input"
  end
  error("bad token " .. tostring(t.tag))
end

local single = { ["("] = "open", [")"] = "close", [","] = "comma", ["="] = "assign" }
local operators = { ["+"] = true, ["-"] = true, ["*"] = true, ["/"] = true, ["%"] = true, ["^"] = true }

local function tokenize(src)
  local tokens = {}
  local pos = 1
  while true do
    local _, blank_end = src:find("^%s*", pos)
    pos = blank_end + 1
    if pos > #src then break end
    local number = src:match("^%d+%.?%d*", pos)
    local word = src:match("^[%a_][%w_]*", pos)
    local token
    if number then
      pos = pos + #number
      token = { tag = "num", value = tonumber(number) }
    elseif word then
      pos = pos + #word
      token = { tag = "ident", name = word }
    else
      local c = src:sub(pos, pos)
      pos = pos + 1
      if single[c] then
        token = { tag = single[c] }
      elseif operators[c] then
        token = { tag = "op", op = c }
      else
        return nil, "unexpected character '" .. c .. "' at column " .. (pos - 1)
      end
    end
    tokens[#tokens + 1] = token
  end
  tokens[#tokens + 1] = { tag = "end" }
  return tokens
end

local binding_power = { ["+"] = 10, ["-"] = 10, ["*"] = 20, ["/"] = 20, ["%"] = 20, ["^"] = 30 }
local NEG_POWER <const> = 25

local Parser = {}
Parser.__index = Parser

function Parser.new(tokens)
  return setmetatable({ tokens = tokens, pos = 1 }, Parser)
end

function Parser:peek() return self.tokens[self.pos] end

function Parser:advance()
  local t = self.tokens[self.pos]
  self.pos = self.pos + 1
  return t
end

function Parser:expect(tag, what)
  local got = self:advance()
  if got.tag ~= tag then
    return nil, "expected " .. what .. ", found " .. describe(got)
  end
  return true
end

function Parser:expr(min)
  local lhs, err = self:prefix()
  if lhs == nil then return nil, err end
  while true do
    local t = self:peek()
    if t.tag ~= "op" then break end
    local power = binding_power[t.op]
    if power <= min then break end
    self:advance()
    local rhs
    rhs, err = self:expr(t.op == "^" and power - 1 or power)
    if rhs == nil then return nil, err end
    lhs = { tag = "bin", op = t.op, lhs = lhs, rhs = rhs }
  end
  return lhs
end

function Parser:prefix()
  local t = self:advance()
  if t.tag == "num" then
    return { tag = "num", value = t.value }
  elseif t.tag == "ident" then
    if self:peek().tag ~= "open" then
      return { tag = "var", name = t.name }
    end
    self:advance()
    local args = {}
    if self:peek().tag ~= "close" then
      while true do
        local a, err = self:expr(0)
        if a == nil then return nil, err end
        args[#args + 1] = a
        if self:peek().tag ~= "comma" then break end
        self:advance()
      end
    end
    local ok, err = self:expect("close", "')'")
    if not ok then return nil, err end
    return { tag = "call", name = t.name, args = args }
  elseif t.tag == "op" and t.op == "-" then
    local inner, err = self:expr(NEG_POWER)
    if inner == nil then return nil, err end
    return { tag = "neg", inner = inner }
  elseif t.tag == "open" then
    local inner, err = self:expr(0)
    if inner == nil then return nil, err end
    local ok, err2 = self:expect("close", "')'")
    if not ok then return nil, err2 end
    return inner
  end
  return nil, "unexpected " .. describe(t)
end

local function parse_line(line)
  local tokens, err = tokenize(line)
  if tokens == nil then return nil, err end
  local p = Parser.new(tokens)
  local stmt
  if tokens[1].tag == "ident" and tokens[2] and tokens[2].tag == "assign" then
    p.pos = 3
    local value, err2 = p:expr(0)
    if value == nil then return nil, err2 end
    stmt = { tag = "assign", name = tokens[1].name, value = value }
  else
    local value, err2 = p:expr(0)
    if value == nil then return nil, err2 end
    stmt = { tag = "show", value = value }
  end
  local t = p:peek()
  if t.tag ~= "end" then
    return nil, "unexpected " .. describe(t) .. " after the expression"
  end
  return stmt
end

local functions = { max = math.max, min = math.min, sqrt = math.sqrt, abs = math.abs }

local function eval(e, env)
  if e.tag == "num" then
    return e.value
  elseif e.tag == "var" then
    local v = env[e.name]
    if v == nil then return nil, "unknown variable '" .. e.name .. "'" end
    return v
  elseif e.tag == "neg" then
    local v, err = eval(e.inner, env)
    if v == nil then return nil, err end
    return -v
  elseif e.tag == "call" then
    local f = functions[e.name]
    if f == nil then return nil, "unknown function '" .. e.name .. "'" end
    if #e.args == 0 then return nil, e.name .. " needs at least one argument" end
    local values = {}
    for i, a in ipairs(e.args) do
      local v, err = eval(a, env)
      if v == nil then return nil, err end
      values[i] = v
    end
    return f(table.unpack(values))
  elseif e.tag == "bin" then
    local a, err = eval(e.lhs, env)
    if a == nil then return nil, err end
    local b, err2 = eval(e.rhs, env)
    if b == nil then return nil, err2 end
    local op = e.op
    if op == "+" then return a + b
    elseif op == "-" then return a - b
    elseif op == "*" then return a * b
    elseif (op == "/" or op == "%") and b == 0 then return nil, "division by zero"
    elseif op == "/" then return a / b
    elseif op == "%" then return a % b
    elseif op == "^" then return a ^ b
    end
  end
  error("bad node " .. tostring(e.tag))
end

local function run_line(line, env)
  local stmt, err = parse_line(line)
  if stmt == nil then return nil, err end
  local v
  v, err = eval(stmt.value, env)
  if v == nil then return nil, err end
  if stmt.tag == "assign" then
    env[stmt.name] = v
    return stmt.name .. " = " .. tostring(v)
  end
  return tostring(v)
end

local program = {
  "1 + 2 * 3",
  "x = 4",
  "(x + 1) * -2",
  "y = x ^ 2 / 8",
  "2 ^ 3 ^ 2",
  "-2 ^ 2",
  "17 % 5 - 7 % -3",
  "max(x, y, 10 % 4)",
  "sqrt(y * 8) + abs(-pi)",
  "y - z",
  "1 / (x - 4)",
  "2 +",
  "3 $ 4",
  "(1 + 2",
  "foo(1)",
  "max()",
  "x = = 1",
  "x 1",
}

local rounds = tonumber(arg and arg[1]) or 1
for round = 1, rounds do
  local env = { pi = math.pi }
  for _, line in ipairs(program) do
    local result, err = run_line(line, env)
    if round == rounds then
      print("> " .. line)
      print(result ~= nil and result or "error: " .. err)
    end
  end
end
