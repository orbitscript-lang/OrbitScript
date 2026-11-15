/* Shared by the programs in tests/capi: each runs a list of parts, every
   part a C function called through lua_pcall, and prints what the API
   returns, so the same program can run on the reference liblua and on
   liborbit and their outputs be compared (cargo testkit capi). A program
   prints nothing that may rightly differ between two implementations:
   addresses, memory sizes, the order of a table's keys. */
#ifndef CAPI_H
#define CAPI_H

#include <stdio.h>
#include <string.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"

/* One value as text: numbers, strings and booleans by value, the rest by
   type. */
static inline void capi_value(lua_State *L, int i) {
  int t = lua_type(L, i);
  switch (t) {
    case LUA_TNUMBER:
      if (lua_isinteger(L, i))
        printf("%lld", (long long)lua_tointeger(L, i));
      else
        printf("%.14g", lua_tonumber(L, i));
      break;
    case LUA_TSTRING: {
      size_t n;
      const char *s = lua_tolstring(L, i, &n);
      putchar('\'');
      for (size_t k = 0; k < n; k++) {
        unsigned char c = (unsigned char)s[k];
        if (c >= 32 && c < 127)
          putchar(c);
        else
          printf("\\%d", c);
      }
      putchar('\'');
      break;
    }
    case LUA_TBOOLEAN: printf("%s", lua_toboolean(L, i) ? "true" : "false"); break;
    case LUA_TNONE: printf("none"); break;
    default: printf("%s", lua_typename(L, t));
  }
}

/* The whole stack: "what: top=N: v1 v2 ...". */
static inline void show(lua_State *L, const char *what) {
  int n = lua_gettop(L);
  printf("%s: top=%d:", what, n);
  for (int i = 1; i <= n; i++) {
    putchar(' ');
    capi_value(L, i);
  }
  putchar('\n');
}

/* An error object as the part's result reports it. */
static inline void capi_error(lua_State *L, int st) {
  printf("status %d", st);
  if (st != LUA_OK) {
    printf(": ");
    capi_value(L, -1);
  }
  putchar('\n');
}

struct capi_part {
  const char *name;
  lua_CFunction f;
};

/* Runs each part in a fresh stack of state L, protected, and prints how it
   ended. The parts' table ends with {NULL, NULL}. */
static inline void run_parts(lua_State *L, const struct capi_part *parts) {
  for (const struct capi_part *p = parts; p->name; p++) {
    printf("-- %s\n", p->name);
    lua_settop(L, 0);
    lua_pushcfunction(L, p->f);
    int st = lua_pcall(L, 0, 0, 0);
    printf("-- %s ", p->name);
    capi_error(L, st);
    fflush(stdout);
  }
}

/* A state with the standard libraries, which runs `parts` and closes. */
static inline int capi_main(const struct capi_part *parts) {
  /* Unbuffered, so a program that dies keeps what it printed. */
  setvbuf(stdout, NULL, _IONBF, 0);
  lua_State *L = luaL_newstate();
  luaL_openlibs(L);
  run_parts(L, parts);
  lua_close(L);
  return 0;
}

/* Runs Lua code; on an error prints it. Returns the status. */
static inline int run(lua_State *L, const char *code) {
  int st = luaL_dostring(L, code);
  if (st != LUA_OK) {
    printf("run error: ");
    capi_error(L, st);
    lua_pop(L, 1);
  }
  return st;
}

#endif
