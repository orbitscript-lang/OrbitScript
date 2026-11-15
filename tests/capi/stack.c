/* The stack: pushing, moving and querying values, and what each type
   query says of each kind of value. */
#include "capi.h"

static int cf(lua_State *L) {
  lua_pushvalue(L, lua_upvalueindex(1));
  return 1;
}

static int moves(lua_State *L) {
  for (int i = 1; i <= 5; i++) lua_pushinteger(L, i);
  lua_rotate(L, 2, 2);
  show(L, "rotate 2,2");
  lua_rotate(L, 1, -1);
  show(L, "rotate 1,-1");
  lua_rotate(L, -3, 3);
  show(L, "rotate -3,3");
  lua_insert(L, 1);
  show(L, "insert 1");
  lua_remove(L, 2);
  show(L, "remove 2");
  lua_replace(L, 1);
  show(L, "replace 1");
  lua_copy(L, 1, 3);
  show(L, "copy 1 3");
  lua_pushvalue(L, -2);
  show(L, "pushvalue -2");
  printf("absindex %d %d %d %d\n", lua_absindex(L, -1), lua_absindex(L, 2),
         lua_absindex(L, LUA_REGISTRYINDEX) == LUA_REGISTRYINDEX,
         lua_absindex(L, lua_upvalueindex(1)) == lua_upvalueindex(1));
  lua_settop(L, -3);
  show(L, "settop -3");
  lua_settop(L, 5);
  show(L, "settop 5");
  lua_pop(L, 4);
  show(L, "pop 4");
  printf("checkstack %d", lua_checkstack(L, 100));
  printf(" %d\n", lua_checkstack(L, 2000000000));
  luaL_checkstack(L, 50, "no room");
  for (int i = 0; i < 50; i++) lua_pushinteger(L, i);
  printf("top after 50 pushes %d\n", lua_gettop(L));
  lua_settop(L, 0);
  return 0;
}

static int pushes(lua_State *L) {
  lua_pushnil(L);
  lua_pushboolean(L, 7);
  lua_pushboolean(L, 0);
  lua_pushinteger(L, LUA_MININTEGER);
  lua_pushnumber(L, -0.0);
  lua_pushnumber(L, 1e300 * 1e10);
  lua_pushlstring(L, "a\0b", 3);
  lua_pushlstring(L, NULL, 0);
  printf("pushstring returns its copy: %s\n", lua_pushstring(L, "copy"));
  printf("pushstring NULL: %d\n", lua_pushstring(L, NULL) == NULL);
  lua_pushliteral(L, "lit");
  show(L, "pushes");
  printf("types:");
  for (int i = 1; i <= lua_gettop(L) + 1; i++) printf(" %s", luaL_typename(L, i));
  printf("\n");
  printf("typename none %s\n", lua_typename(L, LUA_TNONE));
  return 0;
}

/* Each query on each kind of value, and on an index past the top. */
static int queries(lua_State *L) {
  lua_pushinteger(L, 3);
  lua_pushnumber(L, 3.5);
  lua_pushnumber(L, 4.0);
  lua_pushliteral(L, "10");
  lua_pushliteral(L, " 0x10p1 ");
  lua_pushliteral(L, "x");
  lua_pushcfunction(L, cf);
  lua_newuserdatauv(L, 8, 1);
  lua_pushlightuserdata(L, L);
  lua_pushnil(L);
  lua_pushboolean(L, 1);
  luaL_loadstring(L, "return 1");
  lua_newtable(L);
  lua_pushthread(L);
  int top = lua_gettop(L);
  for (int i = 1; i <= top + 1; i++) {
    int isnum;
    lua_Integer n = lua_tointegerx(L, i, &isnum);
    int isflt;
    lua_Number d = lua_tonumberx(L, i, &isflt);
    printf("%d %s: number=%d integer=%d string=%d cfunction=%d userdata=%d "
           "function=%d table=%d nil=%d none=%d noneornil=%d bool=%d thread=%d light=%d",
           i, luaL_typename(L, i), lua_isnumber(L, i), lua_isinteger(L, i), lua_isstring(L, i),
           lua_iscfunction(L, i), lua_isuserdata(L, i), lua_isfunction(L, i), lua_istable(L, i),
           lua_isnil(L, i), lua_isnone(L, i), lua_isnoneornil(L, i), lua_isboolean(L, i),
           lua_isthread(L, i), lua_islightuserdata(L, i));
    printf(" tointegerx=%lld,%d tonumberx=%.14g,%d toboolean=%d", (long long)n, isnum, d, isflt,
           lua_toboolean(L, i));
    printf(" tocfunction=%d touserdata=%d tothread=%d topointer=%d\n", lua_tocfunction(L, i) == cf,
           lua_touserdata(L, i) != NULL, lua_tothread(L, i) != NULL, lua_topointer(L, i) != NULL);
  }
  printf("tothread is L: %d\n", lua_tothread(L, top) == L);
  printf("touserdata light is L: %d\n", lua_touserdata(L, 9) == (void *)L);
  printf("topointer same table twice: %d\n", lua_topointer(L, 13) == lua_topointer(L, 13));
  printf("tonumberx NULL: %.14g\n", lua_tonumberx(L, 5, NULL));
  printf("tointeger 4.0: %lld 3.5: %lld\n", (long long)lua_tointeger(L, 3),
         (long long)lua_tointeger(L, 2));
  /* lua_tolstring converts a number in place. */
  size_t len;
  const char *s = lua_tolstring(L, 1, &len);
  printf("tolstring 3 -> %s len %d, now %s\n", s, (int)len, luaL_typename(L, 1));
  printf("tolstring of a table: %d\n", lua_tolstring(L, 13, NULL) == NULL);
  return 0;
}

/* xmove between a thread and its state, and the main thread. */
static int threads(lua_State *L) {
  lua_State *T = lua_newthread(L);
  lua_pushinteger(L, 1);
  lua_pushinteger(L, 2);
  lua_xmove(L, T, 2);
  show(T, "thread after xmove");
  show(L, "state after xmove");
  lua_xmove(T, L, 1);
  show(L, "back");
  lua_rawgeti(L, LUA_REGISTRYINDEX, LUA_RIDX_MAINTHREAD);
  printf("main thread %d, is L %d\n", lua_isthread(L, -1), lua_tothread(L, -1) == L);
  lua_rawgeti(L, LUA_REGISTRYINDEX, LUA_RIDX_GLOBALS);
  lua_pushglobaltable(L);
  printf("globals same %d\n", lua_rawequal(L, -1, -2));
  printf("pushthread main %d", lua_pushthread(L));
  printf(" other %d\n", lua_pushthread(T));
  return 0;
}

static int version(lua_State *L) {
  printf("version %.0f\n", (double)lua_version(L));
  luaL_checkversion(L);
  printf("ident starts '%.9s'\n", lua_ident);
  return 0;
}

static const struct capi_part parts[] = {
    {"moves", moves},     {"pushes", pushes},   {"queries", queries},
    {"threads", threads}, {"version", version}, {NULL, NULL},
};

int main(void) { return capi_main(parts); }
