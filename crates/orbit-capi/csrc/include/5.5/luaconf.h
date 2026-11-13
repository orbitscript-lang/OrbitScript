/*
 * OrbitScript's luaconf.h: the configuration the Lua 5.5 C API headers
 * depend on. Written for OrbitScript; the values match the reference
 * build's ABI on 64-bit targets so modules compiled against either set of
 * headers load in OrbitScript (docs/adr/0008-c-modules.md).
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

/* Number types, as named choices (OrbitScript has only these). */
#define LUA_INT_INT 1
#define LUA_INT_LONG 2
#define LUA_INT_LONGLONG 3
#define LUA_FLOAT_FLOAT 1
#define LUA_FLOAT_DOUBLE 2
#define LUA_FLOAT_LONGDOUBLE 3
#define LUA_INT_DEFAULT LUA_INT_LONGLONG
#define LUA_FLOAT_DEFAULT LUA_FLOAT_DOUBLE
#define LUA_INT_TYPE LUA_INT_DEFAULT
#define LUA_FLOAT_TYPE LUA_FLOAT_DEFAULT
#define LUAI_IS32INT ((UINT_MAX >> 30) >= 3)
#define LUA_INTEGER_FRMLEN "ll"
#define LUA_NUMBER_FRMLEN ""
#define LUA_MAXUNSIGNED ULLONG_MAX
#define LUAI_UACINT LUA_INTEGER
#define LUAI_UACNUMBER double
#define LUA_NUMBER_FMT_N "%.17g"

/* Conversions, for C code that formats numbers as Lua does. */
#define l_mathop(op) op
#define l_floor(x) (l_mathop(floor)(x))
#define l_floatatt(n) (DBL_##n)
#define l_sprintf(s, sz, f, i) snprintf(s, sz, f, i)
#define lua_number2str(s, sz, n) l_sprintf((s), sz, LUA_NUMBER_FMT, (LUAI_UACNUMBER)(n))
#define lua_integer2str(s, sz, n) l_sprintf((s), sz, LUA_INTEGER_FMT, (LUAI_UACINT)(n))
#define lua_number2strx(L, b, sz, f, n) ((void)L, l_sprintf(b, sz, f, (LUAI_UACNUMBER)(n)))
#define lua_str2number(s, p) strtod((s), (p))
#define lua_strx2number(s, p) lua_str2number(s, p)
#define lua_pointer2str(buff, sz, p) l_sprintf(buff, sz, "%p", p)
#define lua_getlocaledecpoint() (localeconv()->decimal_point[0])

/* Module search paths: the defaults `package.path` and `package.cpath`
   start with (when LUA_PATH and LUA_CPATH are unset). */
#if defined(_WIN32)
#define LUA_DIRSEP "\\"
#define LUA_DL_DLL
#else
#define LUA_DIRSEP "/"
#define LUA_USE_POSIX
#define LUA_USE_DLOPEN
#endif
#define LUA_PATH_SEP ";"
#define LUA_PATH_MARK "?"
#define LUA_EXEC_DIR "!"
#define LUA_IGMARK "-"
#define LUA_VDIR LUA_VERSION_MAJOR "." LUA_VERSION_MINOR
#define LUA_ROOT "/usr/local/"
#define LUA_LDIR LUA_ROOT "share/lua/" LUA_VDIR "/"
#define LUA_CDIR LUA_ROOT "lib/lua/" LUA_VDIR "/"
#define LUA_SHRDIR LUA_ROOT "share/orbit/"
#define LUA_PATH_DEFAULT \
  LUA_LDIR "?.lua;" LUA_LDIR "?/init.lua;" LUA_CDIR "?.lua;" LUA_CDIR "?/init.lua;" \
  "./?.lua;./?/init.lua;" LUA_SHRDIR "?.orb;" LUA_SHRDIR "?/init.orb;./?.orb;./?/init.orb"
#define LUA_CPATH_DEFAULT LUA_CDIR "?.so;" LUA_CDIR "loadall.so;./?.so"

/* The global declarations of Lua 5.5 (`global` is a reserved word). */
#define LUA_COMPAT_GLOBAL

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
