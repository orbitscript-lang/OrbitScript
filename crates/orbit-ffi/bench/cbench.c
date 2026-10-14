#include "lua.h"
#include "lauxlib.h"
#include <stdlib.h>
#include <string.h>
#include <math.h>
static int c_labs(lua_State *L) { lua_pushinteger(L, labs(luaL_checkinteger(L, 1))); return 1; }
static int c_strlen(lua_State *L) { const char *s = luaL_checkstring(L, 1); lua_pushinteger(L, (lua_Integer)strlen(s)); return 1; }
static int c_sqrt(lua_State *L) { lua_pushnumber(L, sqrt(luaL_checknumber(L, 1))); return 1; }
static int c_add3(lua_State *L) { lua_pushinteger(L, luaL_checkinteger(L, 1) + luaL_checkinteger(L, 2) + luaL_checkinteger(L, 3)); return 1; }
/* Plain C, for the FFI to call. */
long add3(long a, long b, long c) { return a + b + c; }
int luaopen_cbench(lua_State *L) {
  static const luaL_Reg r[] = {{"labs", c_labs}, {"strlen", c_strlen}, {"sqrt", c_sqrt}, {"add3", c_add3}, {NULL, NULL}};
  luaL_newlib(L, r);
  return 1;
}
