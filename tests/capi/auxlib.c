/* The auxiliary library's helpers: string buffers, references, gsub,
   subtables, libraries and module loading, luaL_tolstring, and the
   results of system calls. */
#include <errno.h>

#include "capi.h"

static int buffers(lua_State *L) {
  luaL_Buffer b;
  luaL_buffinit(L, &b);
  luaL_addchar(&b, '<');
  luaL_addstring(&b, "ab");
  luaL_addlstring(&b, "c\0d", 3);
  luaL_addgsub(&b, "x.y.z", ".", "::");
  lua_pushinteger(L, 12);
  luaL_addvalue(&b);
  char *p = luaL_prepbuffsize(&b, 3);
  memcpy(p, "xyz", 3);
  luaL_addsize(&b, 3);
  printf("bufflen %d\n", (int)luaL_bufflen(&b));
  luaL_buffsub(&b, 1);
  printf("buffaddr starts '%c'\n", luaL_buffaddr(&b)[0]);
  luaL_pushresult(&b);
  show(L, "buffer");
  lua_settop(L, 0);
  /* A buffer larger than any first block. */
  char *big = luaL_buffinitsize(L, &b, 100000);
  memset(big, 'q', 100000);
  luaL_pushresultsize(&b, 100000);
  printf("big buffer %d\n", (int)lua_rawlen(L, -1));
  lua_settop(L, 0);
  luaL_buffinit(L, &b);
  for (int i = 0; i < 5000; i++) luaL_addstring(&b, "0123456789");
  luaL_pushresult(&b);
  printf("grown buffer %d\n", (int)lua_rawlen(L, -1));
  lua_settop(L, 0);
  printf("gsub returns its result: %s\n", luaL_gsub(L, "hello", "l", "LL"));
  luaL_gsub(L, "aaa", "a", "");
  show(L, "gsub");
  return 0;
}

static int refs(lua_State *L) {
  lua_newtable(L);
  lua_pushliteral(L, "a");
  int a = luaL_ref(L, 1);
  lua_pushliteral(L, "b");
  int b = luaL_ref(L, 1);
  lua_pushnil(L);
  int n = luaL_ref(L, 1);
  printf("refs %d %d nil=%d noref=%d top=%d\n", a, b, n == LUA_REFNIL, LUA_NOREF, lua_gettop(L));
  lua_rawgeti(L, 1, a);
  lua_rawgeti(L, 1, b);
  show(L, "values");
  lua_settop(L, 1);
  luaL_unref(L, 1, a);
  lua_pushliteral(L, "c");
  int c = luaL_ref(L, 1);
  printf("reused %d\n", c == a);
  luaL_unref(L, 1, b);
  luaL_unref(L, 1, c);
  luaL_unref(L, 1, LUA_NOREF);
  luaL_unref(L, 1, LUA_REFNIL);
  lua_pushliteral(L, "d");
  int d = luaL_ref(L, 1);
  lua_pushliteral(L, "e");
  int e = luaL_ref(L, 1);
  lua_pushliteral(L, "f");
  int f = luaL_ref(L, 1);
  printf("after freeing two: %d %d %d rawlen %d\n", d, e, f, (int)lua_rawlen(L, 1));
  /* References in the registry. */
  lua_pushliteral(L, "in the registry");
  int r = luaL_ref(L, LUA_REGISTRYINDEX);
  lua_rawgeti(L, LUA_REGISTRYINDEX, r);
  printf("registry ref above the reserved slots: %d, value %s\n", r > LUA_RIDX_LAST,
         lua_tostring(L, -1));
  luaL_unref(L, LUA_REGISTRYINDEX, r);
  lua_rawgeti(L, LUA_REGISTRYINDEX, LUA_RIDX_GLOBALS);
  lua_pushglobaltable(L);
  printf("globals still in place: %d\n", lua_rawequal(L, -1, -2));
  return 0;
}

static int subtables(lua_State *L) {
  lua_newtable(L);
  printf("getsubtable new %d", luaL_getsubtable(L, 1, "s"));
  printf(" again %d", luaL_getsubtable(L, 1, "s"));
  printf(" same %d\n", lua_rawequal(L, -1, -2));
  lua_pushinteger(L, 5);
  lua_setfield(L, 1, "n");
  printf("getsubtable over a number %d\n", luaL_getsubtable(L, 1, "n"));
  return 0;
}

static int add(lua_State *L) {
  lua_pushinteger(L, luaL_checkinteger(L, 1) + lua_tointeger(L, lua_upvalueindex(1)));
  return 1;
}

static int open_mylib(lua_State *L) {
  static const luaL_Reg fns[] = {{"add", add}, {"placeholder", NULL}, {NULL, NULL}};
  luaL_newlibtable(L, fns);
  lua_pushinteger(L, 100);
  luaL_setfuncs(L, fns, 1);
  lua_pushstring(L, luaL_optstring(L, 1, "?"));
  lua_setfield(L, -2, "name");
  return 1;
}

static int open_plain(lua_State *L) {
  static const luaL_Reg fns[] = {{"add", add}, {NULL, NULL}};
  luaL_newlib(L, fns);
  return 1;
}

static int libraries(lua_State *L) {
  luaL_requiref(L, "mylib", open_mylib, 1);
  lua_getfield(L, -1, "name");
  show(L, "requiref");
  lua_settop(L, 0);
  run(L, "print('mylib', mylib.add(1), mylib.name, mylib.placeholder, package.loaded.mylib == mylib)");
  luaL_requiref(L, "mylib", open_plain, 0);
  run(L, "print('again: loaded once', mylib.name)");
  luaL_requiref(L, "plain", open_plain, 0);
  run(L, "print('not global', plain, package.loaded.plain ~= nil)");
  return 0;
}

static int tostrings(lua_State *L) {
  run(L, "return setmetatable({}, {__tostring = function() return 'TS' end}),"
         "  setmetatable({}, {__name = 'NM'}), nil, true, 1.5, 10, 'str', 1e100, -0.0");
  int top = lua_gettop(L);
  for (int i = 1; i <= top; i++) {
    size_t n;
    const char *t = luaL_tolstring(L, i, &n);
    printf("luaL_tolstring %d: %.2s len>0=%d top+1=%d\n", i, t, n > 0, lua_gettop(L) == top + 1);
    lua_pop(L, 1);
  }
  lua_settop(L, 0);
  run(L, "return setmetatable({}, {__tostring = function() return 1 end})");
  luaL_tolstring(L, 1, NULL);
  return 0;
}

static int results(lua_State *L) {
  printf("fileresult ok %d", luaL_fileresult(L, 1, "f"));
  printf(" top %d\n", lua_gettop(L));
  show(L, "ok");
  lua_settop(L, 0);
  errno = ENOENT;
  printf("fileresult fail %d", luaL_fileresult(L, 0, "name"));
  printf(" top %d\n", lua_gettop(L));
  show(L, "fail");
  lua_settop(L, 0);
  errno = EACCES;
  luaL_fileresult(L, 0, NULL);
  show(L, "fail without a name");
  lua_settop(L, 0);
  luaL_execresult(L, 0);
  show(L, "exec 0");
  lua_settop(L, 0);
  luaL_execresult(L, 3 << 8);
  show(L, "exec exit 3");
  lua_settop(L, 0);
  errno = ENOENT;
  luaL_execresult(L, -1);
  show(L, "exec -1");
  return 0;
}

/* External strings are new in 5.5. */
#if LUA_VERSION_NUM >= 505
static void external_free(void *ud, void *p) {
  (void)ud;
  (void)p;
}

static void *free_alloc(void *ud, void *p, size_t osize, size_t nsize) {
  (void)ud, (void)osize;
  if (nsize == 0) {
    external_free(ud, p);
    return NULL;
  }
  return NULL;
}

static int externals(lua_State *L) {
  static const char text[] = "an external string";
  printf("pushexternalstring returns its text: %s\n",
         lua_pushexternalstring(L, text, sizeof text - 1, free_alloc, NULL));
  printf("len %d\n", (int)lua_rawlen(L, -1));
  lua_pushliteral(L, "an external string");
  printf("equal to the same text %d\n", lua_rawequal(L, -1, -2));
  return 0;
}
#endif

static const struct capi_part parts[] = {
    {"buffers", buffers},     {"refs", refs},         {"subtables", subtables},
    {"libraries", libraries}, {"tostrings", tostrings}, {"results", results},
#if LUA_VERSION_NUM >= 505
    {"externals", externals},
#endif
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
