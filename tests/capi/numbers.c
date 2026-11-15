/* Numbers and strings: conversions both ways, arithmetic and comparison
   through the API, concatenation, and lua_pushfstring's formats. */
#include <stdarg.h>

#include "capi.h"

static int stringtonumber(lua_State *L) {
  static const char *const in[] = {
      "10",   " 0x10 ", "1e1",  "0x.8p1", "1 2", "",        "nan",   "1e",
      "  ",   "-0",     "0x",   "1.",     ".5",  "9223372036854775807",
      "9223372036854775808", "0x7fffffffffffffffff", "-9223372036854775808",
      "inf",  "1e400",  "\t5\n", NULL};
  for (int i = 0; in[i]; i++) {
    size_t n = lua_stringtonumber(L, in[i]);
    printf("stringtonumber '%s' -> %d", in[i], (int)n);
    if (n) {
      printf(" %s ", lua_isinteger(L, -1) ? "int" : "float");
      capi_value(L, -1);
      lua_pop(L, 1);
    }
    printf(" top=%d\n", lua_gettop(L));
  }
  return 0;
}

/* lua_numbertocstring is new in 5.5. */
#if LUA_VERSION_NUM >= 505
static int numbertocstring(lua_State *L) {
  char buf[LUA_N2SBUFFSZ];
  lua_pushinteger(L, -42);
  lua_pushnumber(L, 0.1);
  lua_pushnumber(L, 1e100);
  lua_pushnumber(L, 3.0);
  lua_pushnumber(L, -0.0);
  lua_pushinteger(L, LUA_MININTEGER);
  lua_pushnumber(L, 1.0 / 3);
  lua_pushliteral(L, "12");
  lua_pushnil(L);
  for (int i = 1; i <= 9; i++) {
    unsigned n = lua_numbertocstring(L, i, buf);
    printf("numbertocstring %d -> %u '%s'\n", i, n, n ? buf : "");
  }
  printf("buffer size at least 44: %d\n", LUA_N2SBUFFSZ >= 44);
  return 0;
}
#endif

static int arith(lua_State *L) {
  static const char *const names[] = {"add", "sub", "mul", "mod",  "pow",  "div", "idiv",
                                      "band", "bor", "bxor", "shl", "shr", "unm", "bnot"};
  lua_Integer a[] = {7, -7};
  for (int k = 0; k < 2; k++) {
    for (int op = LUA_OPADD; op <= LUA_OPBNOT; op++) {
      lua_pushinteger(L, a[k]);
      if (op != LUA_OPUNM && op != LUA_OPBNOT) lua_pushinteger(L, 2);
      lua_arith(L, op);
      printf("%lld %s 2 = ", (long long)a[k], names[op]);
      capi_value(L, -1);
      printf("\n");
      lua_pop(L, 1);
    }
  }
  lua_pushnumber(L, 7.5);
  lua_pushliteral(L, "2");
  lua_arith(L, LUA_OPADD);
  lua_pushliteral(L, "0x10");
  lua_arith(L, LUA_OPUNM);
  lua_pushnumber(L, 1);
  lua_pushnumber(L, 0);
  lua_arith(L, LUA_OPDIV);
  show(L, "mixed");
  lua_settop(L, 0);
  run(L, "return setmetatable({}, {__add = function(a, b) return 'meta add' end})");
  lua_pushinteger(L, 1);
  lua_arith(L, LUA_OPADD);
  show(L, "metamethod");
  return 0;
}

static int mod_zero(lua_State *L) {
  lua_pushinteger(L, 7);
  lua_pushinteger(L, 0);
  lua_arith(L, LUA_OPMOD);
  return 0;
}

static int arith_table(lua_State *L) {
  lua_newtable(L);
  lua_pushinteger(L, 1);
  lua_arith(L, LUA_OPMUL);
  return 0;
}

static int compare(lua_State *L) {
  lua_pushinteger(L, 1);
  lua_pushnumber(L, 1.0);
  lua_pushliteral(L, "1");
  lua_pushliteral(L, "2");
  lua_pushnumber(L, 0.0 / 0.0 == 0.0 ? 0 : -(0.0 / 0.0 + 1e308 * 10 - 1e308 * 10));
  printf("eq 1 1.0: %d\n", lua_compare(L, 1, 2, LUA_OPEQ));
  printf("le 1 1.0: %d\n", lua_compare(L, 1, 2, LUA_OPLE));
  printf("lt 1 1.0: %d\n", lua_compare(L, 1, 2, LUA_OPLT));
  printf("eq 1 '1': %d\n", lua_compare(L, 1, 3, LUA_OPEQ));
  printf("lt '1' '2': %d\n", lua_compare(L, 3, 4, LUA_OPLT));
  printf("le '2' '1': %d\n", lua_compare(L, 4, 3, LUA_OPLE));
  printf("eq nan nan: %d\n", lua_compare(L, 5, 5, LUA_OPEQ));
  printf("rawequal 1 1.0: %d, 1 '1': %d\n", lua_rawequal(L, 1, 2), lua_rawequal(L, 1, 3));
  printf("invalid index: eq %d rawequal %d\n", lua_compare(L, 1, 20, LUA_OPEQ),
         lua_rawequal(L, 20, 21));
  run(L, "return setmetatable({}, {__eq = function() return true end, "
         "__lt = function() return true end, __le = function() return false end})");
  lua_newtable(L);
  lua_getmetatable(L, -2);
  lua_setmetatable(L, -2);
  printf("meta eq %d lt %d le %d rawequal %d\n", lua_compare(L, -1, -2, LUA_OPEQ),
         lua_compare(L, -1, -2, LUA_OPLT), lua_compare(L, -1, -2, LUA_OPLE),
         lua_rawequal(L, -1, -2));
  return 0;
}

static int compare_mixed(lua_State *L) {
  lua_pushinteger(L, 1);
  lua_pushliteral(L, "1");
  lua_compare(L, 1, 2, LUA_OPLT);
  return 0;
}

static int concat(lua_State *L) {
  lua_pushliteral(L, "a");
  lua_pushinteger(L, 1);
  lua_pushnumber(L, 2.5);
  lua_concat(L, 3);
  show(L, "concat 3");
  lua_concat(L, 1);
  show(L, "concat 1");
  lua_concat(L, 0);
  show(L, "concat 0");
  run(L, "return setmetatable({}, {__concat = function(a, b) return 'cc' end})");
  lua_pushliteral(L, "x");
  lua_concat(L, 2);
  show(L, "metamethod");
  return 0;
}

static int concat_bad(lua_State *L) {
  lua_pushliteral(L, "a");
  lua_pushboolean(L, 1);
  lua_concat(L, 2);
  return 0;
}

static const char *vf(lua_State *L, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  const char *s = lua_pushvfstring(L, fmt, ap);
  va_end(ap);
  return s;
}

static int fstrings(lua_State *L) {
  lua_pushfstring(L, "%d|%s|%c|%%|%I|%U", -3, "abc", 'q', (lua_Integer)1 << 62, 0x10FFFFL);
  lua_pushfstring(L, "%f %f %f %f", 0.1, 1e15, 1e16, -0.0);
  lua_pushfstring(L, "%f %f", 1e300 * 1e10, 3.0);
  lua_pushfstring(L, "%U %U %U %U", 0x41L, 0x7FFL, 0x800L, 0x7FFFFFFFL);
  lua_pushfstring(L, "%d %d", 0x7fffffff, -0x7fffffff - 1);
  lua_pushfstring(L, "%I", (lua_Integer)LUA_MININTEGER);
  lua_pushfstring(L, "%s|%s", "", "x");
  lua_pushfstring(L, "no format");
  lua_pushfstring(L, "%c%c", 0, 'z');
  printf("vfstring returns its copy: %s\n", vf(L, "<%s %d>", "v", 5));
  show(L, "fstrings");
  return 0;
}

static const struct capi_part parts[] = {
    {"stringtonumber", stringtonumber},
#if LUA_VERSION_NUM >= 505
    {"numbertocstring", numbertocstring},
#endif
    {"arith", arith},
    {"mod by zero", mod_zero},
    {"arith on a table", arith_table},
    {"compare", compare},
    {"compare a number and a string", compare_mixed},
    {"concat", concat},
    {"concat a boolean", concat_bad},
    {"fstrings", fstrings},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
