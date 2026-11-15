/* The debug interface from C: lua_getstack, lua_getinfo, lua_getlocal,
   lua_setlocal and hooks (lua_sethook, lua_gethook, lua_gethookmask,
   lua_gethookcount). */
#include "capi.h"

static void describe(lua_State *L, lua_Debug *ar) {
  lua_getinfo(L, "nSlu", ar);
  printf("  %s %s:%d name %s (%s) nups %d nparams %d vararg %d\n", ar->what, ar->short_src,
         ar->currentline, ar->name ? ar->name : "NULL", ar->namewhat, ar->nups, ar->nparams,
         ar->isvararg);
}

/* Called from Lua: walks the stack and shows and changes the caller's
   locals. */
static int inspect(lua_State *L) {
  lua_Debug ar;
  int level = 0;
  while (lua_getstack(L, level, &ar)) {
    printf("level %d\n", level);
    describe(L, &ar);
    level++;
  }
  printf("levels %d\n", level);
  if (!lua_getstack(L, 1, &ar)) return 0;
  for (int n = 1;; n++) {
    const char *name = lua_getlocal(L, &ar, n);
    if (!name) break;
    printf("local %d %s = ", n, name);
    capi_value(L, -1);
    printf("\n");
    lua_pop(L, 1);
  }
  /* Vararg slots have negative numbers. */
  for (int n = -1;; n--) {
    const char *name = lua_getlocal(L, &ar, n);
    if (!name) break;
    printf("vararg %d %s = ", n, name);
    capi_value(L, -1);
    printf("\n");
    lua_pop(L, 1);
  }
  lua_pushliteral(L, "changed");
  const char *set = lua_setlocal(L, &ar, 2);
  printf("setlocal 2 -> %s top %d\n", set ? set : "NULL", lua_gettop(L));
  lua_pushliteral(L, "nowhere");
  set = lua_setlocal(L, &ar, 50);
  printf("setlocal 50 -> %s top %d\n", set ? set : "NULL", lua_gettop(L));
  lua_settop(L, 0);
  return 0;
}

static int stack_and_locals(lua_State *L) {
  lua_pushcfunction(L, inspect);
  lua_setglobal(L, "inspect");
  run(L, "local function probe(a, b, ...)\n"
         "  local c = a + 1\n"
         "  inspect()\n"
         "  return b\n"
         "end\n"
         "print('b is now', probe(1, 'two', 'x', 'y'))");
  /* Parameters of a function not running: getlocal with NULL. */
  luaL_loadstring(L, "local function f(p, q, ...) end return f");
  lua_call(L, 0, 1);
  for (int n = 1; n <= 3; n++) {
    const char *name = lua_getlocal(L, NULL, n);
    printf("parameter %d %s\n", n, name ? name : "NULL");
  }
  lua_settop(L, 0);
  lua_Debug ar;
  printf("getstack past the top: %d\n", lua_getstack(L, 50, &ar));
  /* ">" takes the function from the stack. */
  luaL_loadstring(L, "return 1");
  lua_getinfo(L, ">S", &ar);
  printf("> what %s top %d\n", ar.what, lua_gettop(L));
  lua_pushcfunction(L, inspect);
  lua_getinfo(L, ">Sf", &ar);
  printf("> what %s, pushed the function %d\n", ar.what, lua_tocfunction(L, -1) == inspect);
  return 0;
}

static const char *event_name(int ev) {
  switch (ev) {
    case LUA_HOOKCALL: return "call";
    case LUA_HOOKRET: return "return";
    case LUA_HOOKLINE: return "line";
    case LUA_HOOKCOUNT: return "count";
    case LUA_HOOKTAILCALL: return "tail call";
    default: return "?";
  }
}

static int counted;

static void hook(lua_State *L, lua_Debug *ar) {
  const char *ev = event_name(ar->event);
  if (ar->event == LUA_HOOKCOUNT) {
    counted++;
    return;
  }
  lua_getinfo(L, "nSl", ar);
  /* Only the chunk below, not the functions the hooks see on the way. */
  if (strcmp(ar->what, "C") == 0) {
    printf("  %s C %s\n", ev, ar->name ? ar->name : "?");
    return;
  }
  printf("  %s %s line %d name %s\n", ev, ar->what, ar->currentline, ar->name ? ar->name : "NULL");
}

static int hooks(lua_State *L) {
  printf("no hook: %d mask %d count %d\n", lua_gethook(L) == NULL, lua_gethookmask(L),
         lua_gethookcount(L));
  luaL_loadstring(L, "local function g(x)\n"
                     "  return x * 2\n"
                     "end\n"
                     "local function f(x)\n"
                     "  local y = g(x)\n"
                     "  return g(y)\n"
                     "end\n"
                     "return f(3)");
  lua_sethook(L, hook, LUA_MASKCALL | LUA_MASKRET | LUA_MASKLINE, 0);
  printf("hook set: %d mask %d count %d\n", lua_gethook(L) == hook,
         lua_gethookmask(L) == (LUA_MASKCALL | LUA_MASKRET | LUA_MASKLINE), lua_gethookcount(L));
  lua_call(L, 0, 1);
  lua_sethook(L, NULL, 0, 0);
  show(L, "result");
  printf("cleared: %d mask %d\n", lua_gethook(L) == NULL, lua_gethookmask(L));

  /* A count hook: how often it runs depends on the instructions, so only
     whether it ran is printed. */
  counted = 0;
  lua_sethook(L, hook, LUA_MASKCOUNT, 1);
  printf("count hook: mask %d count %d\n", lua_gethookmask(L) == LUA_MASKCOUNT,
         lua_gethookcount(L));
  run(L, "local s = 0 for i = 1, 100 do s = s + i end");
  lua_sethook(L, NULL, 0, 0);
  printf("count hook ran: %d\n", counted > 0);
  return 0;
}

/* A hook can raise an error, which ends the hooked code. */
static void raising_hook(lua_State *L, lua_Debug *ar) {
  lua_getinfo(L, "l", ar);
  if (ar->currentline == 2) luaL_error(L, "stopped at line %d", ar->currentline);
}

static int hook_errors(lua_State *L) {
  lua_sethook(L, raising_hook, LUA_MASKLINE, 0);
  int st = luaL_loadstring(L, "local a = 1\n"
                              "local b = 2\n"
                              "print('not reached')");
  st = lua_pcall(L, 0, 0, 0);
  lua_sethook(L, NULL, 0, 0);
  capi_error(L, st);
  return 0;
}

static const struct capi_part parts[] = {
    {"stack and locals", stack_and_locals},
    {"hooks", hooks},
    {"hook errors", hook_errors},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
