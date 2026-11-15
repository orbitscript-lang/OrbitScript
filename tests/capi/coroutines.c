/* Threads driven from C: lua_resume and lua_yieldk, continuations of
   lua_callk and lua_pcallk across a yield, lua_isyieldable, lua_status
   and lua_closethread. */
#include "capi.h"

/* Resumes T with the top `narg` values of L moved over as arguments (and
   the function below them too when `start`), and prints the status and the
   values it yielded or returned. */
static int resume_with(lua_State *L, lua_State *T, int start, int narg) {
  lua_xmove(L, T, narg + start);
  int nres = 0;
  int st = lua_resume(T, L, narg, &nres);
  if (st == LUA_OK || st == LUA_YIELD) {
    printf("resume -> status %d nres %d:", st, nres);
    for (int i = lua_gettop(T) - nres + 1; i <= lua_gettop(T); i++) {
      putchar(' ');
      capi_value(T, i);
    }
    lua_pop(T, nres);
    printf(" | status %d top %d\n", lua_status(T), lua_gettop(T));
  } else {
    /* After an error only the error object, on top, is specified. */
    printf("resume -> status %d: ", st);
    capi_value(T, -1);
    printf(" | status %d\n", lua_status(T));
  }
  return st;
}

/* Starts T on the function below the top `narg` values of L. */
static int start(lua_State *L, lua_State *T, int narg) { return resume_with(L, T, 1, narg); }

/* Resumes suspended T with the top `narg` values of L. */
static int resume(lua_State *L, lua_State *T, int narg) { return resume_with(L, T, 0, narg); }

static int lua_coroutine(lua_State *L) {
  lua_State *T = lua_newthread(L);
  printf("new thread status %d top %d\n", lua_status(T), lua_gettop(T));
  luaL_loadstring(L, "local a, b = ...\n"
                     "local c = coroutine.yield(a + b, 'first')\n"
                     "local d, e = coroutine.yield(c * 2)\n"
                     "return d, e, 'done'");
  lua_pushinteger(L, 1);
  lua_pushinteger(L, 2);
  start(L, T, 2);
  lua_pushinteger(L, 21);
  resume(L, T, 1);
  lua_pushliteral(L, "x");
  lua_pushliteral(L, "y");
  resume(L, T, 2);
  /* A finished thread has nothing to resume. */
  int st = resume(L, T, 0);
  printf("dead: status %d\n", st);
  return 0;
}

/* Yields its arguments with lua_yield, which returns straight to the
   resumer: the values passed to the next resume are the function's
   results. */
static int yield_all(lua_State *L) {
  printf("yield_all yieldable %d\n", lua_isyieldable(L));
  return lua_yield(L, lua_gettop(L));
}

static int after_yield(lua_State *L, int status, lua_KContext ctx) {
  printf("continuation: status %d ctx %d top %d\n", status, (int)ctx, lua_gettop(L));
  show(L, "continuation stack");
  lua_pushliteral(L, "from continuation");
  return lua_gettop(L);
}

static int yield_k(lua_State *L) {
  lua_pushliteral(L, "kept");
  return lua_yieldk(L, 1, 42, after_yield);
}

static int c_yields(lua_State *L) {
  lua_State *T = lua_newthread(L);
  lua_pushcfunction(L, yield_all);
  lua_pushinteger(L, 7);
  lua_pushliteral(L, "eight");
  start(L, T, 2);
  lua_pushliteral(L, "back");
  resume(L, T, 1);

  lua_State *K = lua_newthread(L);
  lua_pushcfunction(L, yield_k);
  lua_pushinteger(L, 1);
  start(L, K, 1);
  lua_pushliteral(L, "resumed with");
  resume(L, K, 1);
  return 0;
}

/* lua_callk and lua_pcallk of a Lua function that yields: the C function
   goes on in its continuation once the call returns. */
static int after_call(lua_State *L, int status, lua_KContext ctx) {
  printf("after_call: status %d ctx %d\n", status, (int)ctx);
  show(L, "after_call stack");
  return lua_gettop(L);
}

static int calls_k(lua_State *L) {
  int protected = (int)lua_tointeger(L, 1);
  lua_settop(L, 0);
  luaL_loadstring(L, "local v = coroutine.yield('inside')\n"
                     "if v == 'fail' then error('failed', 0) end\n"
                     "return v, 'returned'");
  if (protected) {
    int st = lua_pcallk(L, 0, LUA_MULTRET, 0, 2, after_call);
    return after_call(L, st, 2);
  }
  lua_callk(L, 0, LUA_MULTRET, 1, after_call);
  return after_call(L, LUA_OK, 1);
}

static int continuations(lua_State *L) {
  const char *answers[] = {"fine", "fine", "fail"};
  for (int i = 0; i < 3; i++) {
    printf("case %d\n", i);
    lua_State *T = lua_newthread(L);
    lua_pushcfunction(L, calls_k);
    lua_pushinteger(L, i > 0);
    start(L, T, 1);
    lua_pushstring(L, answers[i]);
    resume(L, T, 1);
    lua_settop(L, 0);
  }
  /* Without a continuation the same call cannot yield. */
  lua_State *T = lua_newthread(L);
  luaL_loadstring(L, "return pcall(function() coroutine.yield() end)");
  start(L, T, 0);
  return 0;
}

static int is_yieldable(lua_State *L) {
  printf("C function in the main thread: %d\n", lua_isyieldable(L));
  lua_State *T = lua_newthread(L);
  printf("fresh thread: %d\n", lua_isyieldable(T));
  lua_pushcfunction(L, yield_all);
  start(L, T, 0);
  printf("suspended thread: %d\n", lua_isyieldable(T));
  return 0;
}

/* lua_closethread on a thread in error and on one suspended inside a
   to-be-closed variable. The thread can run again afterwards. */
static int close_thread(lua_State *L) {
  lua_State *T = lua_newthread(L);
  luaL_loadstring(L, "error({code = 7})");
  start(L, T, 0);
  int st = lua_closethread(T, L);
  printf("closethread -> %d top %d", st, lua_gettop(T));
  if (lua_gettop(T) > 0) {
    lua_getfield(T, -1, "code");
    printf(" code ");
    capi_value(T, -1);
  }
  printf(" status %d\n", lua_status(T));
  lua_settop(T, 0);
  luaL_loadstring(L, "return 'reused'");
  start(L, T, 0);

  luaL_loadstring(L, "local x <close> = setmetatable({}, {__close = function(_, e)\n"
                     "  print('closed with', e) end})\n"
                     "coroutine.yield('holding')\n"
                     "print('not reached')");
  start(L, T, 0);
  st = lua_closethread(T, L);
  printf("closethread -> %d top %d status %d\n", st, lua_gettop(T), lua_status(T));

  /* A __close that errors: closethread reports the error. */
  luaL_loadstring(L, "local x <close> = setmetatable({}, {__close = function()\n"
                     "  error('in close', 0) end})\n"
                     "coroutine.yield()");
  start(L, T, 0);
  st = lua_closethread(T, L);
  printf("closethread -> %d", st);
  if (st != LUA_OK) {
    printf(" ");
    capi_value(T, -1);
  }
  printf("\n");
  return 0;
}

#if LUA_VERSION_NUM == 504
/* lua_resetthread, 5.4's older lua_closethread(T, NULL). */
static int reset_thread(lua_State *L) {
  lua_State *T = lua_newthread(L);
  luaL_loadstring(L, "error('reset me', 0)");
  start(L, T, 0);
  int st = lua_resetthread(T);
  printf("resetthread -> %d", st);
  if (st != LUA_OK) {
    printf(" ");
    capi_value(T, -1);
  }
  printf(" status %d\n", lua_status(T));
  return 0;
}
#endif

/* lua_toclose and lua_closeslot in a C function. */
static int closer(lua_State *L) {
  printf("__close of %s, error ", lua_tostring(L, lua_upvalueindex(1)));
  capi_value(L, 2);
  printf("\n");
  return 0;
}

static void push_closable(lua_State *L, const char *name) {
  lua_newtable(L);
  lua_newtable(L);
  lua_pushstring(L, name);
  lua_pushcclosure(L, closer, 1);
  lua_setfield(L, -2, "__close");
  lua_setmetatable(L, -2);
}

static int marks(lua_State *L) {
  push_closable(L, "first");
  lua_toclose(L, -1);
  push_closable(L, "second");
  lua_toclose(L, -1);
  lua_pushboolean(L, 0);
  lua_toclose(L, -1); /* false needs no closing */
  printf("marked, top %d\n", lua_gettop(L));
  lua_closeslot(L, 2);
  printf("after closeslot 2: ");
  capi_value(L, 2);
  printf(" top %d\n", lua_gettop(L));
  lua_pushliteral(L, "result");
  return 1; /* "first" is closed on the way out */
}

static int marks_then_error(lua_State *L) {
  push_closable(L, "on error");
  lua_toclose(L, -1);
  lua_pushliteral(L, "the error");
  return lua_error(L);
}

static int to_close(lua_State *L) {
  lua_pushcfunction(L, marks);
  lua_call(L, 0, 1);
  show(L, "returned");
  lua_settop(L, 0);
  lua_pushcfunction(L, marks_then_error);
  int st = lua_pcall(L, 0, 0, 0);
  capi_error(L, st);
  return 0;
}

static const struct capi_part parts[] = {
    {"lua coroutine", lua_coroutine},
    {"C yields", c_yields},
    {"continuations", continuations},
    {"isyieldable", is_yieldable},
    {"closethread", close_thread},
#if LUA_VERSION_NUM == 504
    {"resetthread", reset_thread},
#endif
    {"toclose", to_close},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
