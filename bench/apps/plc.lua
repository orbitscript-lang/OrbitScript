-- plc, pure-Lua cryptography: hashes and a stream cipher over generated
-- data, then repeated X25519. usage: plc.lua ROOT KIB SCALARMULTS
local root, kib, n = arg[1], tonumber(arg[2]), tonumber(arg[3])
package.path = root .. "/plc/?.lua;" .. package.path
local line = "The quick brown fox jumps over the lazy dog. "
local data = line:rep(kib * 1024 // #line)
local function hex(s) return (s:gsub(".", function(c) return ("%02x"):format(c:byte()) end)) end
local key, nonce = ("k"):rep(32), ("n"):rep(12)
print("sha256", hex(require("plc.sha2").sha256(data)):sub(1, 16))
print("sha512", hex(require("plc.sha2").sha512(data)):sub(1, 16))
print("sha3", hex(require("plc.sha3").sha256(data)):sub(1, 16))
print("blake2b", hex(require("plc.blake2b").hash(data)):sub(1, 16))
print("md5", hex(require("plc.md5").hash(data)):sub(1, 16))
print("chacha20", hex(require("plc.chacha20").encrypt(key, 1, nonce, data):sub(-8)))
local ec = require("plc.ec25519")
local k = ("\7"):rep(32)
for _ = 1, n do k = ec.scalarmult(k, ec.base) end
print("x25519", hex(k):sub(1, 16))
