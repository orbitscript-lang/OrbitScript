/* lua_atpanic: an error outside any protected call goes to the panic
   function, which here prints the error and ends the program, as a panic
   function must not return. */
#include <stdlib.h>

#include "capi.h"

static int panic(lua_State *L) {
  printf("panic: ");
  capi_value(L, -1);
  printf("\n");
  exit(0);
}

static int unused(lua_State *L) {
  (void)L;
  return 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  lua_State *L = luaL_newstate();
  lua_CFunction old = lua_atpanic(L, unused);
  printf("luaL_newstate sets a panic function %d\n", old != NULL);
  printf("atpanic returns the last one %d\n", lua_atpanic(L, panic) == unused);
  luaL_openlibs(L);
  /* Errors inside protected calls don't reach it. */
  run(L, "error('caught')");
  lua_pushliteral(L, "unprotected");
  lua_error(L);
  printf("not reached\n");
  return 1;
}
