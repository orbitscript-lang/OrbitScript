-- First-light subset: tables, constructors, length, methods on plain tables.
local t = {10, 20, 30, x = "ex", ["y z"] = true}
print(t[1], t[2], t[3], t.x, t["y z"], #t)
local function three() return 1, 2, 3 end
print(#{three()}, #{three(), three()}, #{(three())})
t[4] = 40; t[#t + 1] = 50
print(#t, t[5])
t[1.0] = "one"; t[2^53] = "big"
print(t[1], t[2^53], t[9007199254740992])
local nested = {a = {b = {c = "deep"}}}
print(nested.a.b.c)
nested.a.b.c = "changed"; nested["a"]["b"].d = 4
print(nested.a.b.c, nested.a.b.d)

-- plain-table objects with method-call syntax
local account = {balance = 100}
function account.deposit(self, v) self.balance = self.balance + v; return self end
function account:withdraw(v)
  if v > self.balance then error("insufficient funds") end
  self.balance = self.balance - v
  return self
end
account:deposit(50):withdraw(30)
print(account.balance, pcall(account.withdraw, account, 1000))

-- identity and equality
local p, q = {}, {}
print(p == q, p == p, rawequal(p, p), type(p))
local keyed = {}
keyed[p] = "p"; keyed[q] = "q"; keyed[true] = "t"; keyed[1.5] = "f"
print(keyed[p], keyed[q], keyed[true], keyed[1.5])

-- table library subset
local list = {}
for k = 1, 5 do table.insert(list, k * k) end
table.insert(list, 1, 0)
print(table.concat(list, ","), table.remove(list), table.remove(list, 1), #list)
print(table.unpack({1, 2, 3}))
print(rawlen({1, 2}), rawget(t, "x"), rawset(t, "z", 26).z)
print(next({}), next({7}))
-- deleting while traversing is allowed
local big = {}
for k = 1, 100 do big["k" .. k] = k end
local sum = 0
for k, v in pairs(big) do big[k] = nil; sum = sum + v end
print(sum, next(big))
