/* States: lua_newstate and the allocator functions, warnings, the
   collector's options, opening each library on its own, lua_ident and
   lua_close. Amounts of memory are never printed: they rightly differ. */
#include <stdlib.h>

#include "capi.h"

static int allocs;

static void *counting_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
  (void)osize;
  (*(int *)ud)++;
  if (nsize == 0) {
    free(ptr);
    return NULL;
  }
  return realloc(ptr, nsize);
}

static lua_State *new_state(lua_Alloc f, void *ud) {
#if LUA_VERSION_NUM >= 505
  return lua_newstate(f, ud, 0x5eed);
#else
  return lua_newstate(f, ud);
#endif
}

static int allocators(lua_State *L) {
  int count = 0;
  lua_State *S = new_state(counting_alloc, &count);
  printf("newstate %d\n", S != NULL);
  void *ud = NULL;
  lua_Alloc f = lua_getallocf(S, &ud);
  printf("getallocf: same function %d same data %d\n", f == counting_alloc, ud == &count);
  luaL_openlibs(S);
  run(S, "x = string.rep('a', 10) .. 'b' return #x");
  printf("result %d\n", (int)lua_tointeger(S, -1));
  lua_setallocf(S, f, &allocs);
  f = lua_getallocf(S, &ud);
  printf("setallocf: same function %d new data %d\n", f == counting_alloc, ud == &allocs);
  lua_setallocf(S, counting_alloc, &count);
  lua_close(S);
  (void)L;
#if LUA_VERSION_NUM >= 505
  /* luaL_alloc is the allocator luaL_newstate uses. */
  void *p = luaL_alloc(NULL, NULL, 0, 16);
  memset(p, 1, 16);
  p = luaL_alloc(NULL, p, 16, 64);
  printf("luaL_alloc grows %d\n", p != NULL && ((unsigned char *)p)[15] == 1);
  printf("luaL_alloc frees %d\n", luaL_alloc(NULL, p, 64, 0) == NULL);
  S = lua_newstate(luaL_alloc, NULL, luaL_makeseed(L));
  luaL_openlibs(S);
  run(S, "return ('ok'):upper()");
  printf("state on luaL_alloc and a made seed: %s\n", lua_tostring(S, -1));
  lua_close(S);
#endif
  return 0;
}

static void warnf(void *ud, const char *msg, int tocont) {
  printf("  warning piece '%s' tocont %d ud %d\n", msg, tocont, ud == &allocs);
}

static int warnings(lua_State *L) {
  lua_State *S = luaL_newstate();
  luaL_openlibs(S);
  lua_setwarnf(S, warnf, &allocs);
  lua_warning(S, "one ", 1);
  lua_warning(S, "message", 0);
  run(S, "warn('from ', 'Lua') warn('@on') warn('x')");
  /* No warning function: warnings are dropped. */
  lua_setwarnf(S, NULL, NULL);
  lua_warning(S, "dropped", 0);
  run(S, "warn('dropped too')");
  lua_close(S);
  (void)L;
  return 0;
}

static int collector(lua_State *L) {
  printf("isrunning %d\n", lua_gc(L, LUA_GCISRUNNING));
  lua_gc(L, LUA_GCSTOP);
  printf("after stop: isrunning %d\n", lua_gc(L, LUA_GCISRUNNING));
  lua_gc(L, LUA_GCRESTART);
  printf("after restart: isrunning %d\n", lua_gc(L, LUA_GCISRUNNING));
  printf("count is positive %d\n", lua_gc(L, LUA_GCCOUNT) > 0);
  int kb = lua_gc(L, LUA_GCCOUNT), b = lua_gc(L, LUA_GCCOUNTB);
  printf("countb below 1024 %d, kb %d\n", b >= 0 && b < 1024, kb >= 0);
#if LUA_VERSION_NUM >= 505
  int mode = lua_gc(L, LUA_GCINC);
  printf("inc: previous is a mode %d\n", mode == LUA_GCINC || mode == LUA_GCGEN);
  printf("gen: previous %s\n", lua_gc(L, LUA_GCGEN) == LUA_GCINC ? "inc" : "gen");
  printf("inc: previous %s\n", lua_gc(L, LUA_GCINC) == LUA_GCINC ? "inc" : "gen");
  lua_gc(L, LUA_GCPARAM, LUA_GCPPAUSE, 150);
  printf("pause set to 150: %d\n", lua_gc(L, LUA_GCPARAM, LUA_GCPPAUSE, -1));
  lua_gc(L, LUA_GCPARAM, LUA_GCPSTEPMUL, 300);
  printf("stepmul set to 300: %d\n", lua_gc(L, LUA_GCPARAM, LUA_GCPSTEPMUL, -1));
  lua_gc(L, mode);
#else
  int mode = lua_gc(L, LUA_GCINC, 0, 0, 0);
  printf("inc: previous is a mode %d\n", mode == LUA_GCINC || mode == LUA_GCGEN);
  printf("gen: previous %s\n", lua_gc(L, LUA_GCGEN, 0, 0) == LUA_GCINC ? "inc" : "gen");
  printf("inc: previous %s\n", lua_gc(L, LUA_GCINC, 0, 0, 0) == LUA_GCINC ? "inc" : "gen");
  lua_gc(L, mode, 0, 0, 0);
#endif
#if LUA_VERSION_NUM == 504
  /* Kept for compatibility: changes nothing, and says so. */
  printf("setcstacklimit -> %d\n", lua_setcstacklimit(L, 100));
#endif
  /* In incremental mode a basic step, as many times as it takes, finishes
     a cycle. The mode is set first: liborbit's states start generational
     (ADR-2), lua5.5's incremental. */
#if LUA_VERSION_NUM >= 505
  mode = lua_gc(L, LUA_GCINC);
#else
  mode = lua_gc(L, LUA_GCINC, 0, 0, 0);
#endif
  int finished = 0;
  for (int i = 0; i < 1000 && !finished; i++) finished = lua_gc(L, LUA_GCSTEP, 0);
  printf("step finishes a cycle %d\n", finished);
#if LUA_VERSION_NUM >= 505
  lua_gc(L, mode);
#else
  lua_gc(L, mode, 0, 0, 0);
#endif
  printf("collect %d\n", lua_gc(L, LUA_GCCOLLECT));
  return 0;
}

/* Each library opened on its own in a state with none. */
static int libraries(lua_State *L) {
  static const struct {
    const char *name;
    lua_CFunction open;
  } libs[] = {
      {LUA_GNAME, luaopen_base},           {LUA_LOADLIBNAME, luaopen_package},
      {LUA_COLIBNAME, luaopen_coroutine},  {LUA_TABLIBNAME, luaopen_table},
      {LUA_IOLIBNAME, luaopen_io},         {LUA_OSLIBNAME, luaopen_os},
      {LUA_STRLIBNAME, luaopen_string},    {LUA_MATHLIBNAME, luaopen_math},
      {LUA_UTF8LIBNAME, luaopen_utf8},     {LUA_DBLIBNAME, luaopen_debug},
  };
  (void)L;
  for (size_t i = 0; i < sizeof libs / sizeof libs[0]; i++) {
    lua_State *S = luaL_newstate();
    luaL_requiref(S, libs[i].name, libs[i].open, 1);
    printf("%s: %s, global %s", libs[i].name, luaL_typename(S, -1),
           lua_getglobal(S, libs[i].name) == LUA_TTABLE ? "table" : "missing");
    lua_settop(S, 0);
    lua_getglobal(S, "print");
    printf(", print %s", luaL_typename(S, -1));
    lua_getglobal(S, "string");
    printf(", string %s\n", luaL_typename(S, -1));
    lua_close(S);
  }
  /* Only some libraries; the others preloaded or left out. */
#if LUA_VERSION_NUM >= 505
  lua_State *S = luaL_newstate();
  luaL_openselectedlibs(S, LUA_GLIBK | LUA_STRLIBK, LUA_MATHLIBK);
  run(S, "print(type(string), type(math), type(table), type(require))\n"
         "print(type(package.preload.math))");
  lua_close(S);
#endif
  return 0;
}

static int closing(lua_State *L) {
  (void)L;
  lua_State *S = luaL_newstate();
  luaL_openlibs(S);
  run(S, "setmetatable({}, {__gc = function() print('finalized at close') end})\n"
         "local t <close> = nil\n"
         "keep = setmetatable({}, {__gc = function() print('kept one finalized') end})");
  printf("closing\n");
  lua_close(S);
  printf("closed\n");
  printf("ident starts with $LuaVersion: %d\n",
         strncmp(lua_ident, "$LuaVersion: ", 13) == 0);
  return 0;
}

static const struct capi_part parts[] = {
    {"allocators", allocators}, {"warnings", warnings}, {"collector", collector},
    {"libraries", libraries},   {"closing", closing},   {NULL, NULL},
};

int main(void) { return capi_main(parts); }
