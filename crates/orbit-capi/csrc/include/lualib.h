/*
 * OrbitScript's lualib.h: names of the standard libraries. Opening them
 * from C (luaL_openlibs, luaopen_*) is not provided: OrbitScript opens its
 * libraries from Rust.
 */
#ifndef lualib_h
#define lualib_h

#include "lua.h"

#define LUA_VERSUFFIX "_5_5"
#define LUA_COLIBNAME "coroutine"
#define LUA_TABLIBNAME "table"
#define LUA_IOLIBNAME "io"
#define LUA_OSLIBNAME "os"
#define LUA_STRLIBNAME "string"
#define LUA_UTF8LIBNAME "utf8"
#define LUA_MATHLIBNAME "math"
#define LUA_DBLIBNAME "debug"
#define LUA_LOADLIBNAME "package"

#endif
