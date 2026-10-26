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

/* orbit_setlocalemode's modes: whose locale os.setlocale changes. */

/*
 * The process's: os.setlocale is the C library's setlocale, as in PUC Lua,
 * and the state follows what the program does to the process's locale
 * (from its next lua_call, lua_pcall or lua_resume). The default for
 * states made with lua_newstate, luaL_newstate and orbit_newstate.
 */
#define ORBIT_LOCALE_PROCESS 0

/*
 * The state's own: os.setlocale changes this state only, and never the
 * process's locale or other states. C functions the state calls run with
 * its locale as their thread's. The default for isolates a Rust program
 * makes.
 */
#define ORBIT_LOCALE_STATE 1

/*
 * Switch L's state to `mode` (or with -1, only ask), keeping the locale
 * scripts see: a switch to the process's takes it on, one to the state's
 * own keeps it. Returns the mode before, or -1 for an unknown mode.
 */
LUA_API int (orbit_setlocalemode)(lua_State *L, int mode);

#endif
