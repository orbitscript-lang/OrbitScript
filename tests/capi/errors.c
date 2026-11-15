/* Errors raised from C and how they read: the auxiliary library's
   argument checks, called from C (no Lua caller) and from Lua, lua_error
   with any value, luaL_error's position, message handlers and status
   codes. */
#include "capi.h"

static int opt(lua_State *L) {
  static const char *const names[] = {"one", "two", NULL};
  lua_pushinteger(L, luaL_checkoption(L, 1, "two", names));
  lua_pushinteger(L, luaL_optinteger(L, 2, 7));
  lua_pushnumber(L, luaL_optnumber(L, 3, 1.5));
  lua_pushstring(L, luaL_optstring(L, 4, "dflt"));
  size_t n;
  const char *s = luaL_optlstring(L, 5, "xyz", &n);
  lua_pushinteger(L, (lua_Integer)n);
  (void)s;
  return 5;
}

static int checks(lua_State *L) {
  lua_pushinteger(L, luaL_checkinteger(L, 1));
  lua_pushnumber(L, luaL_checknumber(L, 2));
  size_t n;
  luaL_checklstring(L, 3, &n);
  lua_pushinteger(L, (lua_Integer)n);
  luaL_checktype(L, 4, LUA_TTABLE);
  luaL_checkany(L, 5);
  return 3;
}

static int typeerr(lua_State *L) { return luaL_typeerror(L, 1, "widget"); }
static int argerr(lua_State *L) { return luaL_argerror(L, 2, "bad thing"); }
static int checkany(lua_State *L) {
  luaL_checkany(L, 1);
  return 0;
}
static int errobj(lua_State *L) {
  lua_newtable(L);
  lua_pushinteger(L, 5);
  lua_setfield(L, -2, "code");
  return lua_error(L);
}
static int errnil(lua_State *L) {
  lua_pushnil(L);
  return lua_error(L);
}
static int errfmt(lua_State *L) {
  return luaL_error(L, "n=%d s=%s f=%f c=%c p%% I=%I", 7, "str", 1.25, 'x', (lua_Integer)-9);
}

/* Each check called by lua_pcall from C, with "zzz" as its argument. */
static int from_c(lua_State *L) {
  static const struct {
    const char *name;
    lua_CFunction f;
  } fs[] = {{"typeerror", typeerr}, {"argerror", argerr},    {"checkany", checkany},
            {"error object", errobj}, {"error nil", errnil}, {"luaL_error", errfmt},
            {"checkoption", opt},   {"checks", checks},      {NULL, NULL}};
  for (int i = 0; fs[i].name; i++) {
    lua_settop(L, 0);
    lua_pushcfunction(L, fs[i].f);
    if (fs[i].f != checkany) lua_pushliteral(L, "zzz");
    int st = lua_pcall(L, lua_gettop(L) - 1, LUA_MULTRET, 0);
    printf("%s: ", fs[i].name);
    capi_error(L, st);
    if (st == LUA_OK) show(L, "  results");
  }
  return 0;
}

/* The same checks named by how Lua calls them. */
static int from_lua(lua_State *L) {
  lua_register(L, "opt", opt);
  lua_register(L, "checks", checks);
  lua_register(L, "typeerr", typeerr);
  lua_register(L, "argerr", argerr);
  lua_register(L, "errfmt", errfmt);
  run(L, "local t = {f = typeerr, g = argerr}\n"
         "local function try(...) print(select(2, pcall(...))) end\n"
         "try(opt)\n"
         "try(opt, 'one', 2.5)\n"
         "try(opt, 'two', '3', 'x')\n"
         "try(opt, 'three')\n"
         "try(t.f, {})\n"
         "try(function() t:g() end)\n"
         "try(function() t.g(1, 2) end)\n"
         "try(function() local f = argerr; f(1, 2) end)\n"
         "try(function() errfmt() end)\n"
         "try(checks, 1.5)\n"
         "try(checks, '0x10', 'x')\n"
         "try(checks, 1, '2', 3, {})\n"
         "try(checks, 1, '2', 3, 4)\n"
         "print(checks(1, '2', 'abc', {}, false))\n"
         "print(opt('one', nil, nil, nil, 'ab'))\n");
  return 0;
}

static int handler(lua_State *L) {
  lua_pushfstring(L, "handled: %s", lua_tostring(L, 1));
  return 1;
}

static int bad_handler(lua_State *L) {
  lua_pushnil(L);
  return lua_error(L);
}

static int statuses(lua_State *L) {
  lua_pushcfunction(L, handler);
  luaL_loadstring(L, "error('boom')");
  printf("with a handler %d", lua_pcall(L, 0, 0, 1));
  printf(": %s\n", lua_tostring(L, -1));
  lua_settop(L, 0);
  lua_pushcfunction(L, bad_handler);
  luaL_loadstring(L, "error('boom')");
  printf("with a failing handler %d\n", lua_pcall(L, 0, 0, 1));
  lua_settop(L, 0);
  luaL_loadstring(L, "error('lvl2', 2)");
  printf("level 2 from C %d", lua_pcall(L, 0, 0, 0));
  printf(": %s\n", lua_tostring(L, -1));
  lua_settop(L, 0);
  luaL_loadstring(L, "error(setmetatable({}, {__tostring = function() return 'TS' end}))");
  printf("table error %d", lua_pcall(L, 0, 0, 0));
  printf(": %s\n", luaL_tolstring(L, -1, NULL));
  lua_settop(L, 0);
  printf("status of L %d\n", lua_status(L));
  return 0;
}

static int where(lua_State *L) {
  luaL_where(L, 0);
  luaL_where(L, 1);
  luaL_where(L, 2);
  show(L, "where from C");
  lua_settop(L, 0);
  lua_register(L, "where", where);
  return 0;
}

static int where_lua(lua_State *L) {
  luaL_where(L, 1);
  return 1;
}

static int where_from_lua(lua_State *L) {
  lua_register(L, "where1", where_lua);
  run(L, "local w = where1()\nprint('where from Lua', w)");
  return 0;
}

static int traceback(lua_State *L) {
  luaL_traceback(L, L, "msg", 0);
  printf("traceback starts '%.25s'\n", lua_tostring(L, -1));
  luaL_traceback(L, L, NULL, 0);
  printf("traceback without a message starts '%.16s'\n", lua_tostring(L, -1));
  lua_State *T = lua_newthread(L);
  luaL_traceback(L, T, "other", 0);
  printf("traceback of a new thread: '%s'\n", lua_tostring(L, -1));
  return 0;
}

static const struct capi_part parts[] = {
    {"from C", from_c},
    {"from Lua", from_lua},
    {"statuses", statuses},
    {"where", where},
    {"where from Lua", where_from_lua},
    {"traceback", traceback},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
