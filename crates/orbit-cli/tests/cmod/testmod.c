/* A C module for tests/c_modules.rs: exercises the C API the way real
 * modules use it. Built against orbit-capi's own headers. */
#include <string.h>

#include "lua.h"
#include "lauxlib.h"

/* Stack basics: returns its arguments reversed, plus the count. */
static int t_reverse(lua_State *L) {
  int n = lua_gettop(L);
  for (int i = 1; i <= n / 2; i++) {
    lua_pushvalue(L, i);
    lua_pushvalue(L, n - i + 1);
    lua_replace(L, i);
    lua_replace(L, n - i + 1);
  }
  lua_pushinteger(L, n);
  return n + 1;
}

/* Type names and conversions of the first argument. */
static int t_describe(lua_State *L) {
  int isnum;
  lua_Integer i = lua_tointegerx(L, 1, &isnum);
  lua_pushstring(L, luaL_typename(L, 1));
  if (isnum) lua_pushinteger(L, i); else lua_pushnil(L);
  lua_pushboolean(L, lua_isstring(L, 1));
  lua_pushinteger(L, (lua_Integer)luaL_len(L, 1));
  return 4;
}

/* Strings with embedded zeros survive both ways. */
static int t_zeros(lua_State *L) {
  size_t len;
  const char *s = luaL_checklstring(L, 1, &len);
  luaL_Buffer b;
  luaL_buffinit(L, &b);
  for (size_t i = 0; i < len; i++) {
    luaL_addchar(&b, s[i] == '\0' ? '0' : s[i]);
  }
  luaL_addlstring(&b, "\0end", 4);
  luaL_pushresult(&b);
  lua_pushinteger(L, (lua_Integer)len);
  return 2;
}

static int t_format(lua_State *L) {
  lua_pushfstring(L, "%s=%d (%f) %c %% %p", luaL_checkstring(L, 1),
                  (int)luaL_checkinteger(L, 2), luaL_checknumber(L, 3), 'x',
                  (void *)0);
  return 1;
}

/* Tables: build {n, name=..., [true]=list} with raw and metamethod access. */
static int t_tables(lua_State *L) {
  luaL_checktype(L, 1, LUA_TTABLE);
  lua_getfield(L, 1, "key"); /* may run __index */
  lua_createtable(L, 3, 1);
  for (int i = 1; i <= 3; i++) {
    lua_pushinteger(L, i * 10);
    lua_rawseti(L, -2, i);
  }
  lua_pushliteral(L, "named");
  lua_setfield(L, -2, "name");
  lua_pushvalue(L, -1);
  lua_setfield(L, 1, "copied"); /* may run __newindex */
  int count = 0;
  lua_pushnil(L);
  while (lua_next(L, -2)) {
    count++;
    lua_pop(L, 1);
  }
  lua_pushinteger(L, count);
  return 3;
}

/* Calls back into Lua: f(a, b) through lua_call. */
static int t_call(lua_State *L) {
  luaL_checktype(L, 1, LUA_TFUNCTION);
  lua_settop(L, 3);
  lua_call(L, 2, LUA_MULTRET);
  return lua_gettop(L);
}

static int handler(lua_State *L) {
  lua_pushfstring(L, "handled: %s", lua_tostring(L, 1));
  return 1;
}

/* pcall with a message handler: returns status, message. */
static int t_pcall(lua_State *L) {
  luaL_checktype(L, 1, LUA_TFUNCTION);
  lua_pushcfunction(L, handler);
  lua_pushvalue(L, 1);
  int status = lua_pcall(L, 0, 1, 2);
  lua_pushinteger(L, status);
  lua_insert(L, -2);
  return 2;
}

/* Errors: a table value, a luaL_error with position, an argument error. */
static int t_error(lua_State *L) {
  const char *how = luaL_checkstring(L, 1);
  if (strcmp(how, "table") == 0) {
    lua_newtable(L);
    lua_pushliteral(L, "payload");
    lua_setfield(L, -2, "what");
    return lua_error(L);
  }
  if (strcmp(how, "aux") == 0) return luaL_error(L, "aux error %d", 42);
  return luaL_argerror(L, 1, "bad how");
}

/* A counter closure with one C upvalue. */
static int counter_step(lua_State *L) {
  lua_Integer n = lua_tointeger(L, lua_upvalueindex(1)) + 1;
  lua_pushinteger(L, n);
  lua_copy(L, -1, lua_upvalueindex(1));
  return 1;
}

static int t_counter(lua_State *L) {
  lua_pushinteger(L, luaL_optinteger(L, 1, 0));
  lua_pushcclosure(L, counter_step, 1);
  return 1;
}

/* Userdata with a metatable, user values and __gc. */
typedef struct {
  double x, y;
} Point;

static int gc_count = 0;

static Point *checkpoint(lua_State *L) {
  return (Point *)luaL_checkudata(L, 1, "testmod.Point");
}

static int point_new(lua_State *L) {
  Point *p = (Point *)lua_newuserdatauv(L, sizeof(Point), 2);
  p->x = luaL_checknumber(L, 1);
  p->y = luaL_checknumber(L, 2);
  luaL_setmetatable(L, "testmod.Point");
  lua_pushliteral(L, "first");
  lua_setiuservalue(L, -2, 1);
  lua_pushliteral(L, "second");
  lua_setiuservalue(L, -2, 2);
  return 1;
}

static int point_sum(lua_State *L) {
  Point *p = checkpoint(L);
  lua_pushnumber(L, p->x + p->y);
  return 1;
}

static int point_uv(lua_State *L) {
  checkpoint(L);
  int t = lua_getiuservalue(L, 1, (int)luaL_checkinteger(L, 2));
  lua_pushinteger(L, t);
  return 2;
}

static int point_gc(lua_State *L) {
  checkpoint(L);
  gc_count++;
  return 0;
}

static int t_gccount(lua_State *L) {
  lua_pushinteger(L, gc_count);
  return 1;
}

/* Registry references. */
static int t_ref(lua_State *L) {
  lua_settop(L, 1);
  int r = luaL_ref(L, LUA_REGISTRYINDEX);
  lua_rawgeti(L, LUA_REGISTRYINDEX, r);
  luaL_unref(L, LUA_REGISTRYINDEX, r);
  lua_pushboolean(L, r != LUA_REFNIL);
  return 2;
}

/* Loading and running a chunk from C. */
static int t_load(lua_State *L) {
  size_t len;
  const char *src = luaL_checklstring(L, 1, &len);
  int status = luaL_loadbuffer(L, src, len, "=testchunk");
  if (status != LUA_OK) {
    lua_pushinteger(L, status);
    lua_insert(L, -2);
    return 2;
  }
  lua_call(L, 0, 1);
  lua_pushinteger(L, LUA_OK);
  lua_insert(L, -2);
  return 2;
}

/* Arithmetic and comparison with metamethods. */
static int t_arith(lua_State *L) {
  lua_settop(L, 2);
  lua_pushvalue(L, 1);
  lua_pushvalue(L, 2);
  lua_arith(L, LUA_OPADD);
  lua_pushboolean(L, lua_compare(L, 1, 2, LUA_OPLT));
  lua_pushboolean(L, lua_rawequal(L, 1, 2));
  return 3;
}

static const luaL_Reg point_methods[] = {
  {"sum", point_sum},
  {"uv", point_uv},
  {NULL, NULL}
};

static const luaL_Reg funcs[] = {
  {"reverse", t_reverse},
  {"describe", t_describe},
  {"zeros", t_zeros},
  {"format", t_format},
  {"tables", t_tables},
  {"call", t_call},
  {"pcall", t_pcall},
  {"error", t_error},
  {"counter", t_counter},
  {"point", point_new},
  {"gccount", t_gccount},
  {"ref", t_ref},
  {"load", t_load},
  {"arith", t_arith},
  {NULL, NULL}
};

int luaopen_testmod(lua_State *L) {
  luaL_checkversion(L);
  if (luaL_newmetatable(L, "testmod.Point")) {
    luaL_newlib(L, point_methods);
    lua_setfield(L, -2, "__index");
    lua_pushcfunction(L, point_gc);
    lua_setfield(L, -2, "__gc");
  }
  lua_pop(L, 1);
  luaL_newlib(L, funcs);
  return 1;
}
