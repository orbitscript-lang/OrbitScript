/* A C program that embeds OrbitScript through the Lua 5.5 C API, as a
   program built for liblua does (ADR-10). Each mode prints what it checks;
   tests/host.rs compares the output. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"

static void run(lua_State *L, const char *code) {
  if (luaL_dostring(L, code) != LUA_OK) {
    printf("error: %s\n", lua_tostring(L, -1));
    lua_pop(L, 1);
  }
  fflush(stdout);
}

static int basic(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  run(L, "print('hello from ' .. _VERSION)\n"
         "function add(a, b) return a + b end\n"
         "setmetatable({}, {__gc = function() print('finalized at close') end})");
  lua_getglobal(L, "add");
  lua_pushinteger(L, 40);
  lua_pushinteger(L, 2);
  lua_call(L, 2, 1);
  printf("add: %lld, top %d\n", (long long)lua_tointeger(L, -1), lua_gettop(L));
  lua_pop(L, 1);
  if (luaL_loadstring(L, "return 1 +") != LUA_OK) printf("syntax: %s\n", lua_tostring(L, -1));
  lua_pop(L, 1);
  lua_pushcfunction(L, luaopen_base);
  printf("pcall error: %d\n", lua_pcall(L, 0, 0, 0) == LUA_OK ? 0 : 1);
  lua_settop(L, 0);
  printf("top %d\n", lua_gettop(L));
  fflush(stdout);
  lua_close(L);
  printf("closed\n");
  return 0;
}

static int selected(void) {
  lua_State *L = luaL_newstate();
  luaL_openselectedlibs(L, LUA_GLIBK | LUA_LOADLIBK | LUA_STRLIBK, LUA_MATHLIBK);
  run(L, "print(type(string), type(math), type(table), type(io))\n"
         "local m = require 'math'\n"
         "print(type(m), m.max(3, 7), type(math))");
  lua_close(L);
  return 0;
}

static int on_panic(lua_State *L) {
  printf("panic: %s\n", lua_tostring(L, -1));
  fflush(stdout);
  exit(0);
  return 0;
}

static int panic(void) {
  lua_State *L = luaL_newstate();
  lua_atpanic(L, on_panic);
  lua_pushstring(L, "unprotected");
  lua_error(L);
  printf("not reached\n");
  return 1;
}

static char warned[256];

static void on_warn(void *ud, const char *msg, int tocont) {
  strcat(warned, msg);
  strcat(warned, tocont ? "+" : "|");
  (void)ud;
}

static int warn(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_setwarnf(L, on_warn, NULL);
  run(L, "warn('@on') warn('a', 'b') warn('c')");
  lua_warning(L, "from ", 1);
  lua_warning(L, "C", 0);
  printf("warnings: %s\n", warned);
  lua_close(L);
  return 0;
}

static long max_rss_kb(void) {
  struct rusage ru;
  getrusage(RUSAGE_SELF, &ru);
  return ru.ru_maxrss;
}

static int strings(void) {
  lua_State *L = luaL_newstate();
  const char *kept;
  long before, i;
  luaL_openlibs(L);
  /* A string's pointer stays valid while it is on the stack, even after
     it moves and other strings come and go. */
  lua_pushstring(L, "kept string");
  kept = lua_tostring(L, -1);
  lua_pushinteger(L, 1);
  lua_insert(L, 1);
  for (i = 0; i < 1000; i++) {
    lua_pushfstring(L, "temporary %d", (int)i);
    (void)lua_tostring(L, -1);
    lua_pop(L, 1);
  }
  printf("kept: %s\n", kept);
  /* Copies of strings no longer on the stack are freed. */
  before = max_rss_kb();
  for (i = 0; i < 2000000; i++) {
    lua_pushfstring(L, "string number %d with some padding", (int)i);
    (void)lua_tostring(L, -1);
    lua_pop(L, 1);
  }
  printf("bounded: %s\n", max_rss_kb() - before < 40 * 1024 ? "yes" : "no");
  if (getenv("HOST_DEBUG")) printf("grew %ld KiB\n", max_rss_kb() - before);
  printf("kept: %s\n", kept);
  lua_close(L);
  return 0;
}

static int gc(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_gc(L, LUA_GCPARAM, LUA_GCPPAUSE, 150);
  printf("pause: %d\n", lua_gc(L, LUA_GCPARAM, LUA_GCPPAUSE, -1));
  printf("gen (was inc): %d\n", lua_gc(L, LUA_GCGEN) == LUA_GCINC);
  printf("inc (was gen): %d\n", lua_gc(L, LUA_GCINC) == LUA_GCGEN);
  lua_gc(L, LUA_GCSTOP);
  printf("running after stop: %d\n", lua_gc(L, LUA_GCISRUNNING));
  lua_gc(L, LUA_GCRESTART);
  printf("running after restart: %d\n", lua_gc(L, LUA_GCISRUNNING));
  printf("collect: %d\n", lua_gc(L, LUA_GCCOLLECT));
  lua_close(L);
  return 0;
}

static void *my_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
  (void)ud;
  (void)osize;
  if (nsize == 0) {
    free(ptr);
    return NULL;
  }
  return realloc(ptr, nsize);
}

static int alloc(void) {
  static int tag;
  void *ud = NULL;
  lua_State *L = lua_newstate(my_alloc, &tag, 0);
  printf("allocator kept: %d\n", lua_getallocf(L, &ud) == my_alloc && ud == &tag);
  luaL_openlibs(L);
  /* A plain state drops warnings. */
  run(L, "warn('@on') warn('dropped') print('ok')");
  lua_close(L);
  return 0;
}

static int states(void) {
  lua_State *a = luaL_newstate(), *b = luaL_newstate();
  luaL_openlibs(a);
  luaL_openlibs(b);
  run(a, "x = 'a'");
  run(b, "x = 'b'");
  run(a, "print(x)");
  run(b, "print(x)");
  lua_close(a);
  run(b, "print(x .. ' still open')");
  lua_close(b);
  return 0;
}

static void line_hook(lua_State *L, lua_Debug *ar) {
  int line = ar->currentline;
  if (ar->event != LUA_HOOKLINE) return;
  if (lua_getinfo(L, "Sl", ar))
    printf("line %d (getinfo %d) in %s\n", line, ar->currentline, ar->short_src);
}

static int counts;

static void stop_hook(lua_State *L, lua_Debug *ar) {
  (void)ar;
  if (++counts < 100) return;
  /* As lua.c's Ctrl-C hook does. */
  lua_sethook(L, NULL, 0, 0);
  luaL_error(L, "stopped after %d", counts);
}

static int hooks(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_sethook(L, line_hook, LUA_MASKLINE, 0);
  printf("hook set: %d mask %d count %d\n", lua_gethook(L) == line_hook,
         lua_gethookmask(L), lua_gethookcount(L));
  if (luaL_loadbuffer(L, "local a = 1\nlocal b = 2\nreturn a + b", 36, "=chunk") ||
      lua_pcall(L, 0, 1, 0))
    printf("error: %s\n", lua_tostring(L, -1));
  else
    printf("result %d\n", (int)lua_tointeger(L, -1));
  lua_settop(L, 0);
  lua_sethook(L, NULL, 0, 0);
  printf("hook cleared: %d\n", lua_gethook(L) == NULL);
  lua_sethook(L, stop_hook, LUA_MASKCOUNT, 1);
  run(L, "print(pcall(function() while true do end end)) print('continued')");
  printf("hook after stop: %d\n", lua_gethook(L) == NULL);
  lua_close(L);
  return 0;
}

static int peek(lua_State *L) {
  lua_Debug ar;
  const char *name;
  int i;
  if (!lua_getstack(L, 1, &ar)) return 0;
  for (i = 1; (name = lua_getlocal(L, &ar, i)) != NULL; i++) {
    printf("local %d %s = %s\n", i, name, luaL_tolstring(L, -1, NULL));
    lua_pop(L, 2);
  }
  lua_pushinteger(L, 99);
  name = lua_setlocal(L, &ar, 1);
  printf("set %s\n", name ? name : "(none)");
  lua_pushinteger(L, 5);
  name = lua_setlocal(L, &ar, 50);
  printf("set past the end: %s, top %d\n", name ? name : "NULL", lua_gettop(L));
  lua_pop(L, 1);
  return 0;
}

static int locals(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_register(L, "peek", peek);
  run(L, "local function f(a, b) local c = a + b; peek(); return a, c end print(f(1, 2))");
  run(L, "function g(x, y) local z end");
  lua_getglobal(L, "g");
  printf("param 2: %s\n", lua_getlocal(L, NULL, 2));
  printf("param 3: %s, top %d\n", lua_getlocal(L, NULL, 3) ? "?" : "NULL", lua_gettop(L));
  lua_close(L);
  return 0;
}

static void closable(lua_State *L, const char *name) {
  lua_getglobal(L, "closable");
  lua_pushstring(L, name);
  lua_call(L, 1, 1);
  lua_toclose(L, -1);
}

static int with_tbc(lua_State *L) {
  int fail = lua_toboolean(L, 1);
  closable(L, "a");
  closable(L, "b");
  closable(L, "c");
  lua_closeslot(L, -1);
  printf("closed slot is nil: %d\n", lua_isnil(L, -1));
  lua_settop(L, 2); /* drops b's slot, closing it */
  printf("after settop\n");
  if (fail) luaL_error(L, "boom");
  lua_pushinteger(L, 7);
  return 1; /* a closes on the way out */
}

static int bad_tbc(lua_State *L) {
  lua_pushinteger(L, 1);
  lua_toclose(L, -1);
  return 0;
}

static int toclose(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_register(L, "with_tbc", with_tbc);
  lua_register(L, "bad_tbc", bad_tbc);
  run(L, "function closable(name)\n"
         "  return setmetatable({}, {__close = function(_, e) print('close', name, e) end})\n"
         "end\n"
         "print(with_tbc(false))\n"
         "print(pcall(with_tbc, true))\n"
         "print(pcall(bad_tbc))");
  lua_close(L);
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "basic") == 0) return basic();
  if (strcmp(mode, "selected") == 0) return selected();
  if (strcmp(mode, "panic") == 0) return panic();
  if (strcmp(mode, "warn") == 0) return warn();
  if (strcmp(mode, "strings") == 0) return strings();
  if (strcmp(mode, "gc") == 0) return gc();
  if (strcmp(mode, "alloc") == 0) return alloc();
  if (strcmp(mode, "states") == 0) return states();
  if (strcmp(mode, "hooks") == 0) return hooks();
  if (strcmp(mode, "locals") == 0) return locals();
  if (strcmp(mode, "toclose") == 0) return toclose();
  fprintf(stderr, "unknown mode %s\n", mode);
  return 2;
}
