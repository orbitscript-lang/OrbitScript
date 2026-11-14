/* A module whose Lua API calls go to another copy of the runtime: the
   liborbit named by ORBIT_LIBORBIT, opened privately, while the static
   orbit that loads the module made the state. This is what happens when a
   module's lua55.dll (or liblua dylib) is not the program's own runtime,
   as in build.sh's --static-programs layout (ADR-10): each call must still
   reach the runtime that made the state. */
#include <dlfcn.h>
#include <stdlib.h>

#include "lauxlib.h"
#include "lua.h"

static void (*other_createtable)(lua_State *, int, int);
static void (*other_pushinteger)(lua_State *, lua_Integer);
static void (*other_pushcclosure)(lua_State *, lua_CFunction, int);
static void (*other_setfield)(lua_State *, int, const char *);
static lua_Integer (*other_checkinteger)(lua_State *, int);
static int (*other_error)(lua_State *, const char *, ...);

static void *find(void *lib, const char *name) {
  void *p = dlsym(lib, name);
  if (p == NULL) abort();
  return p;
}

static int add(lua_State *L) {
  other_pushinteger(L, other_checkinteger(L, 1) + other_checkinteger(L, 2));
  return 1;
}

static int fail(lua_State *L) {
  return other_error(L, "failed in the other copy (%d)", 7);
}

LUAMOD_API int luaopen_crossmod(lua_State *L) {
  const char *path = getenv("ORBIT_LIBORBIT");
  void *lib = path != NULL ? dlopen(path, RTLD_NOW | RTLD_LOCAL) : NULL;
  if (lib == NULL) abort();
  *(void **)&other_createtable = find(lib, "lua_createtable");
  *(void **)&other_pushinteger = find(lib, "lua_pushinteger");
  *(void **)&other_pushcclosure = find(lib, "lua_pushcclosure");
  *(void **)&other_setfield = find(lib, "lua_setfield");
  *(void **)&other_checkinteger = find(lib, "luaL_checkinteger");
  *(void **)&other_error = find(lib, "luaL_error");
  other_createtable(L, 0, 3);
  other_pushinteger(L, 42);
  other_setfield(L, -2, "answer");
  other_pushcclosure(L, add, 0);
  other_setfield(L, -2, "add");
  other_pushcclosure(L, fail, 0);
  other_setfield(L, -2, "fail");
  return 1;
}
