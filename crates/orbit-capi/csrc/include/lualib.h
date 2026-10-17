/*
 * OrbitScript's lualib.h: the standard libraries' names, the masks
 * luaL_openselectedlibs takes, and the functions that open each one. A
 * program that creates its own state (ADR-10) opens them from C; the
 * `orbit` program opens them from Rust.
 */
#ifndef lualib_h
#define lualib_h

#include "lua.h"

#define LUA_VERSUFFIX "_" LUA_VERSION_MAJOR "_" LUA_VERSION_MINOR

#define LUA_GLIBK 1
#define LUA_LOADLIBNAME "package"
#define LUA_LOADLIBK (LUA_GLIBK << 1)
#define LUA_COLIBNAME "coroutine"
#define LUA_COLIBK (LUA_LOADLIBK << 1)
#define LUA_DBLIBNAME "debug"
#define LUA_DBLIBK (LUA_COLIBK << 1)
#define LUA_IOLIBNAME "io"
#define LUA_IOLIBK (LUA_DBLIBK << 1)
#define LUA_MATHLIBNAME "math"
#define LUA_MATHLIBK (LUA_IOLIBK << 1)
#define LUA_OSLIBNAME "os"
#define LUA_OSLIBK (LUA_MATHLIBK << 1)
#define LUA_STRLIBNAME "string"
#define LUA_STRLIBK (LUA_OSLIBK << 1)
#define LUA_TABLIBNAME "table"
#define LUA_TABLIBK (LUA_STRLIBK << 1)
#define LUA_UTF8LIBNAME "utf8"
#define LUA_UTF8LIBK (LUA_TABLIBK << 1)

LUAMOD_API int (luaopen_base)(lua_State *L);
LUAMOD_API int (luaopen_package)(lua_State *L);
LUAMOD_API int (luaopen_coroutine)(lua_State *L);
LUAMOD_API int (luaopen_debug)(lua_State *L);
LUAMOD_API int (luaopen_io)(lua_State *L);
LUAMOD_API int (luaopen_math)(lua_State *L);
LUAMOD_API int (luaopen_os)(lua_State *L);
LUAMOD_API int (luaopen_string)(lua_State *L);
LUAMOD_API int (luaopen_table)(lua_State *L);
LUAMOD_API int (luaopen_utf8)(lua_State *L);

/* Open the libraries in `load`, and put those in `preload` in
   package.preload for `require` to open. */
LUALIB_API void (luaL_openselectedlibs)(lua_State *L, int load, int preload);
#define luaL_openlibs(L) luaL_openselectedlibs(L, ~0, 0)

#endif
