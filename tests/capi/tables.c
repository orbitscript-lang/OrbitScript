/* Tables through the API: reads and writes with and without metamethods,
   raw access by key, integer and pointer, metatables, length, traversal,
   and the errors invalid keys raise. */
#include "capi.h"

static const char *const META =
    "return setmetatable({}, {"
    "  __index = function(t, k) return tostring(k) .. '!' end,"
    "  __newindex = function(t, k, v) rawset(t, k, tostring(v) .. '?') end,"
    "  __len = function() return 42 end})";

static int metamethods(lua_State *L) {
  run(L, META);
  lua_pushliteral(L, "a");
  printf("gettable type %d\n", lua_gettable(L, 1));
  printf("getfield type %d\n", lua_getfield(L, 1, "b"));
  printf("geti type %d\n", lua_geti(L, 1, 3));
  show(L, "reads");
  lua_settop(L, 1);
  lua_pushliteral(L, "k");
  lua_pushliteral(L, "v");
  lua_settable(L, 1);
  lua_pushinteger(L, 5);
  lua_setfield(L, 1, "f");
  lua_pushboolean(L, 1);
  lua_seti(L, 1, 7);
  show(L, "after writes");
  lua_pushliteral(L, "k");
  printf("rawget type %d\n", lua_rawget(L, 1));
  printf("rawgeti type %d\n", lua_rawgeti(L, 1, 7));
  lua_getfield(L, 1, "f");
  show(L, "raw reads");
  lua_settop(L, 1);
  lua_len(L, 1);
  printf("len %lld rawlen %d", (long long)lua_tointeger(L, -1), (int)lua_rawlen(L, 1));
  printf(" luaL_len %lld\n", (long long)luaL_len(L, 1));
  lua_pop(L, 1);
  lua_pushliteral(L, "r");
  lua_pushinteger(L, 1);
  lua_rawset(L, 1);
  lua_pushliteral(L, "r");
  lua_rawget(L, 1);
  show(L, "rawset");
  return 0;
}

static int raw(lua_State *L) {
  static const char key1 = 0, key2 = 0;
  lua_createtable(L, 4, 4);
  for (int i = 1; i <= 4; i++) {
    lua_pushinteger(L, i * 10);
    lua_rawseti(L, 1, i);
  }
  lua_pushliteral(L, "p1");
  lua_rawsetp(L, 1, &key1);
  printf("rawgetp type %d", lua_rawgetp(L, 1, &key1));
  printf(" %d\n", lua_rawgetp(L, 1, &key2));
  show(L, "rawgetp");
  lua_settop(L, 1);
  printf("rawlen %d", (int)lua_rawlen(L, 1));
  printf(" len %lld\n", (long long)luaL_len(L, 1));
  lua_pushnil(L);
  lua_rawseti(L, 1, 4);
  printf("rawlen after removing 4: %d\n", (int)lua_rawlen(L, 1));
  lua_pushliteral(L, "abc");
  lua_len(L, -1);
  printf("string len %lld rawlen %d\n", (long long)lua_tointeger(L, -1), (int)lua_rawlen(L, -2));
  lua_settop(L, 1);
  printf("rawequal %d %d\n", lua_rawequal(L, 1, 1), lua_rawequal(L, 1, 2));
  /* Integer-valued float keys are the integer keys. */
  lua_pushnumber(L, 2.0);
  lua_rawget(L, 1);
  lua_pushnumber(L, 2.0);
  lua_gettable(L, 1);
  show(L, "float keys");
  return 0;
}

static int metatables(lua_State *L) {
  lua_newtable(L);
  printf("getmetatable none %d", lua_getmetatable(L, 1));
  printf(" top %d\n", lua_gettop(L));
  lua_newtable(L);
  lua_pushliteral(L, "M");
  lua_setfield(L, 2, "__name");
  printf("setmetatable %d", lua_setmetatable(L, 1));
  printf(" top %d\n", lua_gettop(L));
  printf("getmetatable %d\n", lua_getmetatable(L, 1));
  lua_getfield(L, -1, "__name");
  show(L, "metatable name");
  lua_settop(L, 1);
  lua_pushnil(L);
  lua_setmetatable(L, 1);
  printf("after removing: %d\n", lua_getmetatable(L, 1));
  /* Strings share one metatable. */
  lua_pushliteral(L, "s");
  printf("string metatable %d\n", lua_getmetatable(L, -1));
  lua_getfield(L, -1, "__index");
  lua_getglobal(L, "string");
  printf("its __index is string: %d\n", lua_rawequal(L, -1, -2));
  lua_settop(L, 0);
  lua_pushinteger(L, 1);
  printf("number metatable %d\n", lua_getmetatable(L, 1));
  return 0;
}

static int traversal(lua_State *L) {
  run(L, "return {10, 20, 30, x = 1, y = 2}");
  int count = 0;
  lua_Integer sum = 0;
  lua_pushnil(L);
  while (lua_next(L, 1)) {
    count++;
    sum += lua_tointeger(L, -1);
    lua_pop(L, 1);
  }
  printf("next visited %d, values sum %lld, top %d\n", count, (long long)sum, lua_gettop(L));
  /* Clearing fields during traversal is allowed. */
  count = 0;
  lua_pushnil(L);
  while (lua_next(L, 1)) {
    count++;
    lua_pop(L, 1);
    lua_pushvalue(L, -1);
    lua_pushnil(L);
    lua_rawset(L, 1);
  }
  lua_pushnil(L);
  printf("cleared %d, next on empty %d", count, lua_next(L, 1));
  printf(" top %d\n", lua_gettop(L));
  return 0;
}

static int nil_key(lua_State *L) {
  lua_newtable(L);
  lua_pushnil(L);
  lua_pushinteger(L, 1);
  lua_rawset(L, 1);
  return 0;
}

static int nan_key(lua_State *L) {
  /* A NaN made at run time: MSVC refuses a constant division by zero. */
  volatile double zero = 0.0;
  lua_newtable(L);
  lua_pushnumber(L, zero / zero);
  lua_pushinteger(L, 1);
  lua_settable(L, 1);
  return 0;
}

static int bad_next(lua_State *L) {
  lua_newtable(L);
  lua_pushliteral(L, "nokey");
  lua_next(L, 1);
  return 0;
}

static int index_nontable(lua_State *L) {
  lua_pushinteger(L, 1);
  lua_getfield(L, 1, "x");
  return 0;
}

static int globals(lua_State *L) {
  lua_pushinteger(L, 9);
  lua_setglobal(L, "g");
  printf("getglobal %d\n", lua_getglobal(L, "g"));
  printf("getglobal missing %d\n", lua_getglobal(L, "missing_global"));
  run(L, "assert(g == 9)");
  show(L, "globals");
  return 0;
}

static const struct capi_part parts[] = {
    {"metamethods", metamethods},
    {"raw", raw},
    {"metatables", metatables},
    {"traversal", traversal},
    {"nil key", nil_key},
    {"nan key", nan_key},
    {"next with a missing key", bad_next},
    {"index a number", index_nontable},
    {"globals", globals},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
