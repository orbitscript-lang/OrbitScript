/*
 * OrbitScript's lualib.h for its Lua 5.4 builds (ADR-14): the standard
 * libraries' names and the functions that open them. A
 * program that creates its own state (ADR-10) opens them from C; the
 * `orbit` program opens them from Rust.
 */
#ifndef lualib_h
#define lualib_h

#include "lua.h"

#define LUA_VERSUFFIX "_" LUA_VERSION_MAJOR "_" LUA_VERSION_MINOR

#define LUA_COLIBNAME "coroutine"
#define LUA_TABLIBNAME "table"
#define LUA_IOLIBNAME "io"
#define LUA_OSLIBNAME "os"
#define LUA_STRLIBNAME "string"
#define LUA_UTF8LIBNAME "utf8"
#define LUA_MATHLIBNAME "math"
#define LUA_DBLIBNAME "debug"
#define LUA_LOADLIBNAME "package"

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

/* Open every standard library. */
LUALIB_API void (luaL_openlibs)(lua_State *L);

#endif
