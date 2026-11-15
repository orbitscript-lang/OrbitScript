/* Loading code: lua_load with a reader in pieces and its modes, the
   auxiliary library's loaders, and lua_dump's output loaded back. */
#include "capi.h"

struct pieces {
  const char *const *p;
};

static const char *reader(lua_State *L, void *ud, size_t *size) {
  struct pieces *r = ud;
  (void)L;
  if (!*r->p) {
    *size = 0;
    return NULL;
  }
  const char *s = *r->p++;
  *size = strlen(s);
  return s;
}

static int load(lua_State *L) {
  static const char *const pieces[] = {"local a = ", "4", "0 retu", "rn a + ", "2", NULL};
  struct pieces r = {pieces};
  printf("load %d\n", lua_load(L, reader, &r, "=pieces", NULL));
  lua_call(L, 0, 1);
  show(L, "pieces");
  static const char *const bad[] = {"x = = 1", NULL};
  struct pieces r2 = {bad};
  printf("load bad %d\n", lua_load(L, reader, &r2, "@file.lua", "t"));
  show(L, "bad");
  static const char *const text[] = {"return 1", NULL};
  struct pieces r3 = {text};
  printf("load mode b on text %d\n", lua_load(L, reader, &r3, "=m", "b"));
  show(L, "mode");
  static const char *const empty[] = {NULL};
  struct pieces r4 = {empty};
  printf("load empty %d\n", lua_load(L, reader, &r4, "=empty", NULL));
  lua_call(L, 0, LUA_MULTRET);
  show(L, "empty");
  lua_settop(L, 0);
  printf("loadbufferx %d\n", luaL_loadbufferx(L, "return ...", 10, "=(buf)", "t"));
  lua_pushinteger(L, 5);
  lua_pushinteger(L, 6);
  lua_call(L, 2, LUA_MULTRET);
  show(L, "varargs");
  lua_settop(L, 0);
  printf("loadbuffer named by the text %d\n", luaL_loadbuffer(L, "error('x')", 10, "error('x')"));
  printf("pcall %d", lua_pcall(L, 0, 0, 0));
  printf(": %s\n", lua_tostring(L, -1));
  lua_settop(L, 0);
  printf("loadstring syntax error %d", luaL_loadstring(L, "return +"));
  printf(": %s\n", lua_tostring(L, -1));
  lua_settop(L, 0);
  printf("dostring %d", luaL_dostring(L, "return 1, 2"));
  printf(" top %d\n", lua_gettop(L));
  return 0;
}

struct bytes {
  char data[4096];
  size_t n;
  int calls;
};

static int writer(lua_State *L, const void *p, size_t size, void *ud) {
  struct bytes *b = ud;
  (void)L;
  if (b->n + size > sizeof b->data) return 1;
  memcpy(b->data + b->n, p, size);
  b->n += size;
  b->calls++;
  return 0;
}

static int failing_writer(lua_State *L, const void *p, size_t size, void *ud) {
  (void)L, (void)p, (void)size, (void)ud;
  return 7;
}

static int dump(lua_State *L) {
  static struct bytes b;
  luaL_loadstring(L, "local x = ... return x * 2, 'kept'");
  for (int strip = 0; strip <= 1; strip++) {
    b.n = 0;
    b.calls = 0;
    printf("dump strip=%d: %d", strip, lua_dump(L, writer, &b, strip));
    printf(" top %d, writer called %d\n", lua_gettop(L), b.calls > 0);
    printf("starts with ESC: %d\n", b.n > 0 && b.data[0] == 27);
    printf("load as text %d\n", luaL_loadbufferx(L, b.data, b.n, "=dumped", "t"));
    lua_settop(L, 1);
    printf("load as binary %d\n", luaL_loadbufferx(L, b.data, b.n, "=dumped", "b"));
    lua_pushinteger(L, 21);
    lua_call(L, 1, 2);
    show(L, "dumped function");
    lua_settop(L, 1);
  }
  printf("dump with a failing writer: %d\n", lua_dump(L, failing_writer, NULL, 0));
  /* A stripped function has no line information. */
  b.n = 0;
  lua_dump(L, writer, &b, 1);
  luaL_loadbufferx(L, b.data, b.n, "=stripped", "b");
  lua_Debug ar;
  lua_getinfo(L, ">Sl", &ar);
  printf("stripped: source %s linedefined %d\n", ar.source, ar.linedefined);
  return 0;
}

static int files(lua_State *L) {
  FILE *f = fopen("capi-load.lua", "w");
  fputs("#!/usr/bin/env lua\nreturn 'from a file', ...\n", f);
  fclose(f);
  printf("loadfile %d\n", luaL_loadfile(L, "capi-load.lua"));
  lua_pushliteral(L, "arg");
  lua_call(L, 1, LUA_MULTRET);
  show(L, "file");
  lua_settop(L, 0);
  printf("loadfilex mode b %d", luaL_loadfilex(L, "capi-load.lua", "b"));
  printf(": %s\n", lua_tostring(L, -1));
  lua_settop(L, 0);
  printf("dofile %d", luaL_dofile(L, "capi-load.lua"));
  printf(" top %d\n", lua_gettop(L));
  lua_settop(L, 0);
  f = fopen("capi-load.lua", "w");
  fputs("\xEF\xBB\xBFreturn 'after a BOM'\n", f);
  fclose(f);
  printf("dofile %d\n", luaL_dofile(L, "capi-load.lua"));
  show(L, "bom");
  lua_settop(L, 0);
  remove("capi-load.lua");
  printf("missing file %d", luaL_loadfile(L, "capi-missing.lua"));
  printf(": %s\n", lua_tostring(L, -1));
  return 0;
}

static const struct capi_part parts[] = {
    {"load", load},
    {"dump", dump},
    {"files", files},
    {NULL, NULL},
};

int main(void) { return capi_main(parts); }
