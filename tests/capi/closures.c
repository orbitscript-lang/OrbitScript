/* C closures and their upvalues, and upvalues of Lua functions seen and
   changed from C. */
#include "capi.h"

static int cf(lua_State *L) {
  lua_pushvalue(L, lua_upvalueindex(1));
  lua_pushvalue(L, lua_upvalueindex(2));
  return 2;
}

static int counter(lua_State *L) {
  lua_Integer n = lua_tointeger(L, lua_upvalueindex(1)) + 1;
  lua_pushinteger(L, n);
  lua_copy(L, -1, lua_upvalueindex(1));
  return 1;
}

static int c_upvalues(lua_State *L) {
  lua_pushliteral(L, "up1");
  lua_pushinteger(L, 2);
  lua_pushcclosure(L, cf, 2);
  printf("iscfunction %d tocfunction %d\n", lua_iscfunction(L, 1), lua_tocfunction(L, 1) == cf);
  for (int n = 0; n <= 3; n++) {
    const char *name = lua_getupvalue(L, 1, n);
    printf("getupvalue %d -> %s", n, name ? (*name ? name : "(empty)") : "NULL");
    printf(" top=%d\n", lua_gettop(L));
    lua_settop(L, 1);
  }
  lua_pushliteral(L, "new");
  const char *set = lua_setupvalue(L, 1, 1);
  printf("setupvalue 1 -> %s top=%d\n", set ? (*set ? set : "(empty)") : "NULL", lua_gettop(L));
  lua_pushliteral(L, "new");
  set = lua_setupvalue(L, 1, 9);
  printf("setupvalue 9 -> %s top=%d\n", set ? set : "NULL", lua_gettop(L));
  lua_settop(L, 1);
  lua_call(L, 0, 2);
  show(L, "closure result");
  lua_settop(L, 0);
  lua_pushinteger(L, 10);
  lua_pushcclosure(L, counter, 1);
  lua_setglobal(L, "counter");
  run(L, "counter() return counter()");
  show(L, "counter");
  lua_settop(L, 0);
  lua_pushcclosure(L, cf, 0);
  printf("no upvalues: getupvalue 1 %s\n", lua_getupvalue(L, 1, 1) ? "found" : "NULL");
  return 0;
}

static int lua_upvalues(lua_State *L) {
  run(L, "local a, b = 1, 2\n"
         "return function() return a, b end, function() b = b + 10; return a, b end");
  const char *n1 = lua_getupvalue(L, 1, 1);
  const char *n2 = lua_getupvalue(L, 1, 2);
  printf("names %s %s\n", n1, n2);
  show(L, "values");
  lua_settop(L, 2);
  /* Upvalues are numbered in order of first use: the second function's
     first is b, its second a. */
  printf("names of the second: %s", lua_getupvalue(L, 2, 1));
  printf(" %s\n", lua_getupvalue(L, 2, 2));
  lua_settop(L, 2);
  printf("same a: %d\n", lua_upvalueid(L, 1, 1) == lua_upvalueid(L, 2, 2));
  printf("same b: %d\n", lua_upvalueid(L, 1, 2) == lua_upvalueid(L, 2, 1));
  printf("a is not b: %d\n", lua_upvalueid(L, 1, 1) != lua_upvalueid(L, 1, 2));
  printf("upvalueid of a missing upvalue: %d\n", lua_upvalueid(L, 1, 5) == NULL);
  /* The first function's a becomes the second's b. */
  lua_upvaluejoin(L, 1, 1, 2, 1);
  lua_pushvalue(L, 2);
  lua_call(L, 0, 0);
  lua_pushvalue(L, 1);
  lua_call(L, 0, 2);
  show(L, "after join");
  lua_settop(L, 2);
  printf("joined id same: %d\n", lua_upvalueid(L, 1, 1) == lua_upvalueid(L, 2, 1));
  lua_pushinteger(L, 99);
  printf("setupvalue -> %s\n", lua_setupvalue(L, 2, 1));
  lua_pushvalue(L, 2);
  lua_call(L, 0, 2);
  show(L, "after set");
  lua_settop(L, 0);
  /* A function's first upvalue is _ENV when it uses globals. */
  luaL_loadstring(L, "return x");
  lua_newtable(L);
  lua_pushinteger(L, 77);
  lua_setfield(L, -2, "x");
  printf("setupvalue env -> %s\n", lua_setupvalue(L, 1, 1));
  lua_call(L, 0, 1);
  show(L, "env");
  return 0;
}

static int closure_with_many(lua_State *L) {
  luaL_checkstack(L, 256, "upvalues");
  for (int i = 0; i < 255; i++) lua_pushinteger(L, i);
  lua_pushcclosure(L, cf, 255);
  lua_getupvalue(L, 1, 255);
  show(L, "255 upvalues");
  return 0;
}

static const struct capi_part parts[] = {
    {"C upvalues", c_upvalues},
    {"Lua upvalues", lua_upvalues},
    {"many upvalues", closure_with_many},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
