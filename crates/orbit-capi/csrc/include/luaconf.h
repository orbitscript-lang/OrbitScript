/*
 * OrbitScript's luaconf.h: the configuration the Lua 5.5 C API headers
 * depend on. Written for OrbitScript; the values match the reference
 * build's ABI on 64-bit targets so modules compiled against either set of
 * headers load in OrbitScript (docs/design/c-modules.md).
 */
#ifndef luaconf_h
#define luaconf_h

#include <limits.h>
#include <stddef.h>
#include <stdint.h>

/* Numbers: 64-bit integers and doubles. */
#define LUA_INTEGER long long
#define LUA_UNSIGNED unsigned long long
#define LUA_NUMBER double
#define LUA_INTEGER_FMT "%lld"
#define LUA_NUMBER_FMT "%.14g"
#define LUA_MAXINTEGER LLONG_MAX
#define LUA_MININTEGER LLONG_MIN

/* Continuation contexts. */
#define LUA_KCONTEXT intptr_t

/* Bytes of user memory in front of each lua_State (lua_getextraspace). */
#define LUA_EXTRASPACE (sizeof(void *))

/* Size of lua_Debug's short_src. */
#define LUA_IDSIZE 60

/* Size of a luaL_Buffer's inline storage. */
#define LUAL_BUFFERSIZE ((int)(16 * sizeof(void *) * sizeof(lua_Number)))

/* Members of a union that has the strictest alignment Lua guarantees. */
#define LUAI_MAXALIGN long double u; void *s; long long l

#if defined(_WIN32)
#if defined(ORBIT_CAPI_BUILD)
#define LUA_API __declspec(dllexport)
#else
#define LUA_API __declspec(dllimport)
#endif
#else
#define LUA_API extern
#endif
#define LUALIB_API LUA_API
#define LUAMOD_API LUA_API

#if defined(__GNUC__)
#define luai_likely(x) (__builtin_expect(((x) != 0), 1))
#define luai_unlikely(x) (__builtin_expect(((x) != 0), 0))
#else
#define luai_likely(x) (x)
#define luai_unlikely(x) (x)
#endif

#endif
