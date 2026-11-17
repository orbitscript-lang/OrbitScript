/* A few POSIX process calls, as luaposix provides them, for the tests of
 * signals in processes a script forks (Unix only). */

#include <signal.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "lua.h"
#include "lauxlib.h"

static int p_fork(lua_State *L) {
  lua_pushinteger(L, (lua_Integer)fork());
  return 1;
}

static int p_kill(lua_State *L) {
  pid_t pid = (pid_t)luaL_checkinteger(L, 1);
  luaL_argcheck(L, pid > 0, 1, "not a process");
  lua_pushinteger(L, kill(pid, (int)luaL_checkinteger(L, 2)));
  return 1;
}

static int p_sleep(lua_State *L) {
  double s = luaL_checknumber(L, 1);
  struct timespec t;
  t.tv_sec = (time_t)s;
  t.tv_nsec = (long)((s - (double)t.tv_sec) * 1e9);
  nanosleep(&t, NULL);
  return 0;
}

/* How a child ended: "exit", code or "signal", number; with a true
 * second argument, nothing if it is still running. */
static int p_wait(lua_State *L) {
  int status;
  pid_t r = waitpid((pid_t)luaL_checkinteger(L, 1), &status,
                    lua_toboolean(L, 2) ? WNOHANG : 0);
  if (r < 0) return luaL_error(L, "waitpid failed");
  if (r == 0) return 0;
  if (WIFSIGNALED(status)) {
    lua_pushliteral(L, "signal");
    lua_pushinteger(L, WTERMSIG(status));
  } else {
    lua_pushliteral(L, "exit");
    lua_pushinteger(L, WEXITSTATUS(status));
  }
  return 2;
}

static int p_exit(lua_State *L) {
  _exit((int)luaL_optinteger(L, 1, 0));
  return 0;
}

static const luaL_Reg funcs[] = {
  {"fork", p_fork},
  {"kill", p_kill},
  {"sleep", p_sleep},
  {"wait", p_wait},
  {"exit", p_exit},
  {NULL, NULL}
};

LUAMOD_API int luaopen_posixmod(lua_State *L) {
  luaL_checkversion(L);
  luaL_newlib(L, funcs);
  return 1;
}
