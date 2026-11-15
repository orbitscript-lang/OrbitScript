/* Full and light userdata: blocks, user values, metatables by name
   (luaL_newmetatable and friends) and metamethods called from C. */
#include "capi.h"

static int blocks(lua_State *L) {
  unsigned char *u = lua_newuserdatauv(L, 16, 3);
  memset(u, 0xAB, 16);
  printf("getiuservalue 1: %d\n", lua_getiuservalue(L, 1, 1));
  printf("getiuservalue 3: %d\n", lua_getiuservalue(L, 1, 3));
  printf("getiuservalue 4: %d\n", lua_getiuservalue(L, 1, 4));
  printf("getiuservalue 0: %d\n", lua_getiuservalue(L, 1, 0));
  show(L, "absent user values");
  lua_settop(L, 1);
  lua_pushinteger(L, 5);
  printf("setiuservalue 3: %d", lua_setiuservalue(L, 1, 3));
  printf(" top %d\n", lua_gettop(L));
  lua_pushinteger(L, 6);
  printf("setiuservalue 4: %d", lua_setiuservalue(L, 1, 4));
  printf(" top %d\n", lua_gettop(L));
  lua_pushliteral(L, "first");
  lua_setiuservalue(L, 1, 1);
  lua_getiuservalue(L, 1, 1);
  lua_getiuservalue(L, 1, 3);
  show(L, "user values");
  lua_settop(L, 1);
  printf("rawlen %d, block kept %d\n", (int)lua_rawlen(L, 1), u[15] == 0xAB);
  printf("touserdata same %d\n", lua_touserdata(L, 1) == (void *)u);
  lua_pushlightuserdata(L, u);
  printf("light: touserdata same %d, rawequal to full %d\n", lua_touserdata(L, -1) == (void *)u,
         lua_rawequal(L, 1, -1));
  lua_pushlightuserdata(L, u);
  printf("two lights equal %d\n", lua_rawequal(L, -1, -2));
  lua_newuserdatauv(L, 0, 0);
  printf("zero size: rawlen %d", (int)lua_rawlen(L, -1));
  printf(" getiuservalue %d\n", lua_getiuservalue(L, -1, 1));
  return 0;
}

static int named(lua_State *L) {
  printf("newmetatable %d", luaL_newmetatable(L, "Widget"));
  printf(" again %d\n", luaL_newmetatable(L, "Widget"));
  lua_getfield(L, 1, "__name");
  show(L, "names");
  lua_settop(L, 0);
  luaL_getmetatable(L, "Widget");
  luaL_getmetatable(L, "Nothing");
  show(L, "getmetatable by name");
  lua_settop(L, 0);
  lua_newuserdatauv(L, 4, 0);
  luaL_setmetatable(L, "Widget");
  lua_newuserdatauv(L, 4, 0);
  lua_newtable(L);
  printf("testudata %d", luaL_testudata(L, 1, "Widget") != NULL);
  printf(" %d", luaL_testudata(L, 1, "Gadget") != NULL);
  printf(" %d", luaL_testudata(L, 2, "Widget") != NULL);
  printf(" %d\n", luaL_testudata(L, 3, "Widget") != NULL);
  printf("checkudata %d\n", luaL_checkudata(L, 1, "Widget") == lua_touserdata(L, 1));
  printf("getmetafield __name %d", luaL_getmetafield(L, 1, "__name"));
  printf(" nope %d", luaL_getmetafield(L, 1, "nope"));
  printf(" no metatable %d\n", luaL_getmetafield(L, 2, "__name"));
  show(L, "metafields");
  return 0;
}

static int checkudata_bad(lua_State *L) {
  luaL_newmetatable(L, "Widget");
  lua_newuserdatauv(L, 4, 0);
  luaL_checkudata(L, 2, "Widget");
  return 0;
}

static int tostring_meta(lua_State *L) {
  lua_pushfstring(L, "W(%d)", *(int *)lua_touserdata(L, 1));
  return 1;
}

static int callmeta(lua_State *L) {
  luaL_newmetatable(L, "Counter");
  lua_pushcfunction(L, tostring_meta);
  lua_setfield(L, 1, "__tostring");
  int *n = lua_newuserdatauv(L, sizeof(int), 0);
  *n = 41;
  luaL_setmetatable(L, "Counter");
  printf("callmeta __tostring %d", luaL_callmeta(L, 2, "__tostring"));
  printf(" __len %d\n", luaL_callmeta(L, 2, "__len"));
  show(L, "callmeta");
  printf("tolstring %s\n", luaL_tolstring(L, 2, NULL));
  lua_pushinteger(L, 1);
  printf("callmeta on a number %d\n", luaL_callmeta(L, -1, "__tostring"));
  return 0;
}

static int gc_meta(lua_State *L) {
  (void)L;
  printf("finalizer ran\n");
  return 0;
}

static int finalize(lua_State *L) {
  lua_newuserdatauv(L, 1, 0);
  lua_newtable(L);
  lua_pushcfunction(L, gc_meta);
  lua_setfield(L, -2, "__gc");
  lua_setmetatable(L, -2);
  lua_settop(L, 0);
  lua_gc(L, LUA_GCCOLLECT);
  printf("after collect\n");
  return 0;
}

static const struct capi_part parts[] = {
    {"blocks", blocks},
    {"named metatables", named},
    {"checkudata with the wrong type", checkudata_bad},
    {"callmeta", callmeta},
    {"finalize", finalize},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
