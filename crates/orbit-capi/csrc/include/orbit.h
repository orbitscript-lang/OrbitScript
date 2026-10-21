/*
 * OrbitScript's own additions to the Lua 5.5 C API, exported by liborbit
 * under the symbol version ORBIT_0 (ADR-12). A program that uses them
 * links only to liborbit, not to another Lua.
 */
#ifndef orbit_h
#define orbit_h

#include "lua.h"

/* orbit_newstate's flags. */

/* No debugger may ever attach to the state, whatever ORBIT_DEBUG says. */
#define ORBIT_STATE_NO_DEBUGGER 1u

/*
 * lua_newstate with flags: lua_newstate(f, ud, seed) and luaL_newstate
 * behave as orbit_newstate with no flags. A state made without
 * ORBIT_STATE_NO_DEBUGGER can be debugged once the program listens
 * (orbit_debug_listen, or ORBIT_DEBUG=1 in the environment when the state
 * is made); until an editor attaches, it runs as it would undebugged.
 */
LUA_API lua_State *(orbit_newstate)(lua_Alloc f, void *ud, unsigned flags);

/*
 * Let debuggers attach to the program: open its local socket (a private
 * Unix socket in the user's runtime directory), for this state and every
 * other debuggable one. Callable from any thread, at any time, and more
 * than once. `addr` is reserved and must be NULL. 0 on success, -1 if L
 * allows no debugger, liborbit has none, or the socket can't be made.
 */
LUA_API int (orbit_debug_listen)(lua_State *L, const char *addr);

#endif
