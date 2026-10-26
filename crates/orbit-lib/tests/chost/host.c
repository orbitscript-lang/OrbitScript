/* A C program that embeds OrbitScript through the Lua 5.5 C API, as a
   program built for liblua does (ADR-10). Each mode prints what it checks;
   tests/host.rs compares the output. */
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"
#include "orbit.h"

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
  /* States start generational (ADR-2). */
  printf("inc (was gen): %d\n", lua_gc(L, LUA_GCINC) == LUA_GCGEN);
  printf("gen (was inc): %d\n", lua_gc(L, LUA_GCGEN) == LUA_GCINC);
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

/* A C function: 2.5 as this thread's C library formats it. */
static int c_format(lua_State *L) {
  char buf[32];
  snprintf(buf, sizeof buf, "%.1f", 2.5);
  lua_pushstring(L, buf);
  return 1;
}

static void host_sees(const char *when) {
  char buf[32];
  snprintf(buf, sizeof buf, "%.1f", 2.5);
  printf("%s: host formats %s, process numeric %s\n", when, buf, setlocale(LC_NUMERIC, NULL));
}

static int locale(void) {
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  lua_register(L, "c_format", c_format);
  printf("mode %d\n", orbit_setlocalemode(L, -1));
  if (setlocale(LC_NUMERIC, "pt_BR.utf8") == NULL) {
    printf("no pt_BR.utf8\n");
    lua_close(L);
    return 0;
  }
  /* The process's locale, as in PUC Lua: the program's change reaches
     scripts, and theirs reaches the program. */
  run(L, "print(1.5, os.setlocale(nil, 'numeric'), c_format())");
  run(L, "os.setlocale('C', 'numeric')");
  host_sees("after the script's C");
  /* The state's own: the process's locale stays as it is. */
  printf("was %d\n", orbit_setlocalemode(L, ORBIT_LOCALE_STATE));
  run(L, "print(os.setlocale('pt_BR.utf8', 'numeric'), 1.5, c_format())");
  host_sees("after the state's pt_BR");
  printf("bad mode %d\n", orbit_setlocalemode(L, 7));
  lua_close(L);
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

/* ---- threads -------------------------------------------------------- */

static const char *status_name(int s) {
  switch (s) {
    case LUA_OK: return "OK";
    case LUA_YIELD: return "YIELD";
    case LUA_ERRRUN: return "ERRRUN";
    case LUA_ERRMEM: return "ERRMEM";
    case LUA_ERRERR: return "ERRERR";
    default: return "?";
  }
}

/* Resume co with the top narg values of its stack and print the outcome,
   leaving co's stack empty. */
static int resume_print(lua_State *L, lua_State *co, int narg) {
  int nres = -1, i;
  int s = lua_resume(co, L, narg, &nres);
  printf("resume: %s", status_name(s));
  if (s == LUA_OK || s == LUA_YIELD) {
    printf(" nres %d:", nres);
    for (i = nres; i >= 1; i--) printf(" %s", luaL_tolstring(co, -i, NULL)), lua_pop(co, 1);
  } else {
    printf(" error: %s", lua_tostring(co, -1));
  }
  printf(" | status %s", status_name(lua_status(co)));
  if (s == LUA_OK || s == LUA_YIELD) printf(", top %d", lua_gettop(co));
  printf("\n");
  lua_settop(co, 0);
  fflush(stdout);
  return s;
}

static int c_yield(lua_State *L) {
  return lua_yield(L, lua_gettop(L));
}

static int after_yield(lua_State *L, int status, lua_KContext ctx) {
  int n = lua_gettop(L);
  printf("continued: %s ctx %d with %d values\n", status_name(status), (int)ctx, n);
  lua_pushstring(L, "k-done");
  return n + 1;
}

static int c_yieldk(lua_State *L) {
  lua_pushstring(L, "from-c");
  return lua_yieldk(L, 1, 42, after_yield);
}

static int after_call(lua_State *L, int status, lua_KContext ctx) {
  printf("call continued: %s ctx %d top %d\n", status_name(status), (int)ctx, lua_gettop(L));
  if (status != LUA_OK && status != LUA_YIELD) {
    lua_pushfstring(L, "caught: %s", lua_tostring(L, -1));
    return 1;
  }
  return lua_gettop(L);
}

static int c_call(lua_State *L) {
  lua_callk(L, lua_gettop(L) - 1, LUA_MULTRET, 7, after_call);
  return after_call(L, LUA_OK, 7);
}

static int c_pcall(lua_State *L) {
  int s = lua_pcallk(L, lua_gettop(L) - 1, 1, 0, 8, after_call);
  return after_call(L, s, 8);
}

static int c_isyieldable(lua_State *L) {
  lua_pushboolean(L, lua_isyieldable(L));
  return 1;
}

static int threads(void) {
  lua_State *L = luaL_newstate(), *co, *co2;
  int status, nres;
  luaL_openlibs(L);
  lua_register(L, "c_yield", c_yield);
  lua_register(L, "c_yieldk", c_yieldk);
  lua_register(L, "c_call", c_call);
  lua_register(L, "c_pcall", c_pcall);
  lua_register(L, "c_isyieldable", c_isyieldable);
  printf("main yieldable %d, main status %s\n", lua_isyieldable(L), status_name(lua_status(L)));

  /* A generator run from C. */
  co = lua_newthread(L);
  printf("new thread: top %d, type %s, status %s\n", lua_gettop(co),
         luaL_typename(L, -1), status_name(lua_status(co)));
  run(L, "function gen(a, b)\n"
         "  local x = coroutine.yield(a + b)\n"
         "  local y, z = coroutine.yield(x * 2, 'two')\n"
         "  return y .. z, coroutine.isyieldable(), c_isyieldable()\n"
         "end");
  lua_getglobal(co, "gen");
  lua_pushinteger(co, 1);
  lua_pushinteger(co, 2);
  resume_print(L, co, 2);
  lua_pushinteger(co, 10);
  resume_print(L, co, 1);
  lua_pushstring(co, "a");
  lua_pushstring(co, "b");
  resume_print(L, co, 2);
  resume_print(L, co, 0);

  /* The same thread, reused for another function. */
  run(L, "function fails(m) coroutine.yield('before') error(m) end");
  lua_getglobal(co, "fails");
  lua_pushstring(co, "oops");
  resume_print(L, co, 1);
  resume_print(L, co, 0);
  resume_print(L, co, 0);

  /* Values move between threads; a thread's state is the same each time. */
  co2 = lua_newthread(L);
  lua_pushinteger(L, 5);
  lua_pushstring(L, "five");
  lua_xmove(L, co2, 2);
  printf("xmove: L top %d, co2 top %d, %s %s\n", lua_gettop(L), lua_gettop(co2),
         lua_tostring(co2, 1), lua_tostring(co2, 2));
  printf("tothread same: %d", lua_tothread(L, -1) == co2);
  printf(", pushthread on co2: %d", lua_pushthread(co2));
  printf(", main: %d", lua_pushthread(L));
  printf(", pushed is co2: %d\n", lua_tothread(co2, -1) == co2);
  lua_settop(co2, 0);
  lua_pop(L, 2);

  /* C functions that yield, with and without continuations. */
  run(L, "local co = coroutine.wrap(function(a) local x, y = c_yield(a + 1, 'up') return x * 2, y end)\n"
         "print('yield', co(1))\n"
         "print('back', co(10, 'down'))\n"
         "co = coroutine.wrap(function() return c_yieldk() end)\n"
         "print('yieldk', co())\n"
         "print('yieldk back', co('r1', 'r2'))\n"
         "co = coroutine.wrap(function()\n"
         "  return c_call(function(x) local y = coroutine.yield(x) return y + 1, 'ret' end, 5)\n"
         "end)\n"
         "print('callk', co())\n"
         "print('callk back', co(10))\n"
         "co = coroutine.wrap(function()\n"
         "  return c_pcall(function() coroutine.yield('p') error('late', 0) end)\n"
         "end)\n"
         "print('pcallk', co())\n"
         "print('pcallk back', co())\n"
         "print('callk outside', c_call(function(a) return a, 'plain' end, 1))\n"
         "print('yield outside', pcall(c_yield, 1))");

  /* A thread stopped by an error is reset by closing it. resume_print emptied its stack, so
     there is no error object to look at (the reference liblua leaves a stale slot there). */
  printf("closethread after error: %s\n", status_name(lua_closethread(co, L)));
  lua_settop(co, 0);
  printf("after close: status %s, top %d\n", status_name(lua_status(co)), lua_gettop(co));

  /* A C thread driving a C function that yields. */
  lua_getglobal(co, "c_yieldk");
  resume_print(L, co, 0);
  lua_pushinteger(co, 99);
  resume_print(L, co, 1);

  /* Closing a suspended thread runs its pending __close. */
  run(L, "function closer()\n"
         "  local t <close> = setmetatable({}, {__close = function(_, e) print('closed with', e) end})\n"
         "  coroutine.yield('holding')\n"
         "end");
  lua_getglobal(co, "closer");
  resume_print(L, co, 0);
  status = lua_closethread(co, L);
  printf("closethread: %s, top %d\n", status_name(status), lua_gettop(co));

  /* Closing a thread with its error object still on the stack leaves that object on top. */
  co2 = lua_newthread(L);
  luaL_loadstring(co2, "error('closing', 0)");
  status = lua_resume(co2, L, 0, &nres);
  printf("closethread with error object: %s", status_name(status));
  status = lua_closethread(co2, L);
  printf(" -> %s %s\n", status_name(status), lua_tostring(co2, -1));
  lua_pop(L, 1);
  run(L, "local co = coroutine.create(function() error({}) end)\n"
         "local ok, e = coroutine.resume(co)\n"
         "print(ok, type(e))\n"
         "print(coroutine.status(co))");
  lua_close(L);
  return 0;
}

/* Debugging (ADR-12): which states may be listened for, then run the
   script argv[2] in a debuggable state with the remaining arguments. */
static int debug(int argc, char **argv) {
  lua_State *open = orbit_newstate(my_alloc, NULL, 0);
  lua_State *closed = orbit_newstate(my_alloc, NULL, ORBIT_STATE_NO_DEBUGGER);
  lua_State *plain = luaL_newstate();
  int i;
  printf("closed: %d\n", orbit_debug_listen(closed, NULL));
  printf("address: %d\n", orbit_debug_listen(open, "somewhere"));
  printf("listen: %d %d\n", orbit_debug_listen(open, NULL), orbit_debug_listen(plain, NULL));
  fflush(stdout);
  lua_close(closed);
  luaL_openlibs(plain);
  if (luaL_loadfile(plain, argv[2]) != LUA_OK) {
    printf("error: %s\n", lua_tostring(plain, -1));
    return 1;
  }
  for (i = 3; i < argc; i++) lua_pushstring(plain, argv[i]);
  if (lua_pcall(plain, argc - 3, 0, 0) != LUA_OK) {
    printf("error: %s\n", lua_tostring(plain, -1));
    return 1;
  }
  fflush(stdout);
  lua_close(plain);
  lua_close(open);
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
  if (strcmp(mode, "threads") == 0) return threads();
  if (strcmp(mode, "locale") == 0) return locale();
  if (strcmp(mode, "debug") == 0 && argc > 2) return debug(argc, argv);
  fprintf(stderr, "unknown mode %s\n", mode);
  return 2;
}
