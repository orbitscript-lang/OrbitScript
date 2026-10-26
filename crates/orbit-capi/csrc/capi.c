/*
 * The C half of OrbitScript's Lua C API (ADR-8).
 *
 * Every lua_* entry point calls a Rust function (orbit_r_*) that does the
 * work and returns normally. If that function raised a Lua error it sets
 * L->status, and only then, back in C, does the entry point longjmp to the
 * trampoline that called the module's function. A longjmp therefore only
 * ever crosses C frames: the module's own and this file's.
 *
 * The auxiliary library (luaL_*) is written here on top of the lua_* API,
 * from the reference manual's description of each function.
 */
/* MSVC deprecates strerror and fopen in favour of its own _s variants;
   this file sticks to standard C. */
#if defined(_MSC_VER)
#define _CRT_SECURE_NO_WARNINGS
#endif
#include <errno.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"
#include "orbit.h"

struct lua_State {
  void *rs;      /* the Rust bridge */
  jmp_buf *jb;   /* the innermost trampoline's jump target */
  int status;    /* set by Rust when an error is pending */
  /* lua_sethook's request, which Rust installs at its next check. Only
     stored to with atomic stores, so a signal handler can make it. */
  int hookchanged; /* read by Rust (ffi.rs's LState) */
  int hookmask;
  int hookcount;
  lua_Hook hook;
  unsigned char *wake; /* the VM's interrupt flag */
  lua_CFunction panicf; /* lua_atpanic */
  lua_Alloc allocf;     /* the allocator lua_newstate got, unused */
  void *allocud;
};

/* The atomic accesses lua_sethook's request needs, safe in a signal
   handler. GCC and Clang (clang-cl too) have the __atomic builtins. MSVC's
   C compiler has none outside its experimental C11 atomics, so there a
   relaxed access is a volatile one (an aligned int or pointer is never
   torn) and an ordered one is an interlocked exchange, a full barrier. */
#if defined(_MSC_VER) && !defined(__clang__)
#include <intrin.h>
static int load_int(const int *p) { return *(const volatile int *)p; }
static void store_int(int *p, int v) { *(volatile int *)p = v; }
static void store_int_release(int *p, int v) { _InterlockedExchange((volatile long *)p, v); }
static int exchange_int_acquire(int *p, int v) {
  return (int)_InterlockedExchange((volatile long *)p, v);
}
static lua_Hook load_hook(lua_Hook *p) { return *(lua_Hook volatile *)p; }
static void store_hook(lua_Hook *p, lua_Hook f) { *(lua_Hook volatile *)p = f; }
static void store_byte(unsigned char *p, unsigned char v) { *(volatile unsigned char *)p = v; }
#else
static int load_int(const int *p) { return __atomic_load_n(p, __ATOMIC_RELAXED); }
static void store_int(int *p, int v) { __atomic_store_n(p, v, __ATOMIC_RELAXED); }
static void store_int_release(int *p, int v) { __atomic_store_n(p, v, __ATOMIC_RELEASE); }
static int exchange_int_acquire(int *p, int v) {
  return __atomic_exchange_n(p, v, __ATOMIC_ACQUIRE);
}
static lua_Hook load_hook(lua_Hook *p) { return __atomic_load_n(p, __ATOMIC_RELAXED); }
static void store_hook(lua_Hook *p, lua_Hook f) { __atomic_store_n(p, f, __ATOMIC_RELAXED); }
static void store_byte(unsigned char *p, unsigned char v) {
  __atomic_store_n(p, v, __ATOMIC_RELAXED);
}
#endif

/* The Rust entry points, handed over by orbit_capi_newstate so none of
   them is an exported symbol. Fields are in name order and match RTable in
   src/ffi.rs. */
struct orbit_r_table {
  int (*absindex)(lua_State *L, int idx);
  void (*argerror)(lua_State *L, int arg, const char *extramsg);
  void (*arith)(lua_State *L, int op);
  void (*callk)(lua_State *L, int nargs, int nresults, lua_KContext ctx, lua_KFunction k);
  int (*checkstack)(lua_State *L, int n);
  void (*close)(lua_State *L);
  void (*closeslot)(lua_State *L, int idx);
  int (*closethread)(lua_State *L, lua_State *from);
  int (*compare)(lua_State *L, int a, int b, int op);
  void (*concat)(lua_State *L, int n);
  void (*copy)(lua_State *L, int fromidx, int toidx);
  void (*createtable)(lua_State *L, int narr, int nrec);
  int (*error)(lua_State *L);
  int (*gc)(lua_State *L, int what, int a, int b);
  int (*getfield)(lua_State *L, int idx, const char *k);
  int (*getglobal)(lua_State *L, const char *name);
  int (*geti)(lua_State *L, int idx, lua_Integer n);
  int (*getinfo)(lua_State *L, const char *what, lua_Debug *ar);
  int (*getiuservalue)(lua_State *L, int idx, int n);
  const char *(*getlocal)(lua_State *L, const lua_Debug *ar, int n);
  int (*getmetatable)(lua_State *L, int idx);
  int (*getstack)(lua_State *L, int level, lua_Debug *ar);
  int (*gettable)(lua_State *L, int idx);
  int (*gettop)(lua_State *L);
  const char * (*getupvalue)(lua_State *L, int funcindex, int n);
  int (*iscfunction)(lua_State *L, int idx);
  int (*isinteger)(lua_State *L, int idx);
  int (*isnumber)(lua_State *L, int idx);
  int (*isrustudata)(lua_State *L, int idx);
  int (*isstring)(lua_State *L, int idx);
  int (*isuserdata)(lua_State *L, int idx);
  int (*isyieldable)(lua_State *L);
  void (*len)(lua_State *L, int idx);
  int (*load)(lua_State *L, const char *buf, size_t len, const char *chunkname, const char *mode);
  lua_State *(*newthread)(lua_State *L);
  void * (*newuserdatauv)(lua_State *L, size_t sz, int nuvalue);
  int (*next)(lua_State *L, int idx);
  unsigned (*numbertocstring)(lua_State *L, int idx, char *buff);
  int (*openlib)(lua_State *L, int lib);
  int (*pcallk)(lua_State *L, int nargs, int nresults, int errfunc, lua_KContext ctx, lua_KFunction k);
  void (*pushboolean)(lua_State *L, int b);
  void (*pushcclosure)(lua_State *L, lua_CFunction fn, int n);
  void (*pushinteger)(lua_State *L, lua_Integer n);
  void (*pushlightuserdata)(lua_State *L, void *p);
  const char * (*pushlstring)(lua_State *L, const char *s, size_t len);
  void (*pushnil)(lua_State *L);
  void (*pushnumber)(lua_State *L, lua_Number n);
  void (*pushpending)(lua_State *L);
  int (*pushthread)(lua_State *L);
  void (*pushvalue)(lua_State *L, int idx);
  int (*rawequal)(lua_State *L, int a, int b);
  int (*rawget)(lua_State *L, int idx);
  int (*rawgeti)(lua_State *L, int idx, lua_Integer n);
  int (*rawgetp)(lua_State *L, int idx, const void *p);
  lua_Unsigned (*rawlen)(lua_State *L, int idx);
  void (*rawset)(lua_State *L, int idx);
  void (*rawseti)(lua_State *L, int idx, lua_Integer n);
  void (*rawsetp)(lua_State *L, int idx, const void *p);
  int (*resume)(lua_State *L, lua_State *from, int narg, int *nres);
  void (*rotate)(lua_State *L, int idx, int n);
  void (*setfield)(lua_State *L, int idx, const char *k);
  void (*setglobal)(lua_State *L, const char *name);
  void (*seti)(lua_State *L, int idx, lua_Integer n);
  int (*setiuservalue)(lua_State *L, int idx, int n);
  const char *(*setlocal)(lua_State *L, const lua_Debug *ar, int n);
  int (*setmetatable)(lua_State *L, int idx);
  void (*settable)(lua_State *L, int idx);
  void (*settop)(lua_State *L, int idx);
  const char * (*setupvalue)(lua_State *L, int funcindex, int n);
  void (*setwarnf)(lua_State *L, lua_WarnFunction f, void *ud);
  int (*status)(lua_State *L);
  size_t (*stringtonumber)(lua_State *L, const char *s);
  int (*toboolean)(lua_State *L, int idx);
  lua_CFunction (*tocfunction)(lua_State *L, int idx);
  void (*toclose)(lua_State *L, int idx);
  lua_Integer (*tointegerx)(lua_State *L, int idx, int *isnum);
  const char * (*tolstring)(lua_State *L, int idx, size_t *len);
  const char * (*tolstringmeta)(lua_State *L, int idx, size_t *len);
  lua_Number (*tonumberx)(lua_State *L, int idx, int *isnum);
  const void * (*topointer)(lua_State *L, int idx);
  lua_State *(*tothread)(lua_State *L, int idx);
  void * (*touserdata)(lua_State *L, int idx);
  void (*traceback)(lua_State *L, const char *msg, int level);
  int (*type)(lua_State *L, int idx);
  void (*typeerror)(lua_State *L, int arg, const char *tname);
  void (*unsupported)(lua_State *L, const char *what);
  void * (*upvalueid)(lua_State *L, int fidx, int n);
  void (*upvaluejoin)(lua_State *L, int fidx1, int n1, int fidx2, int n2);
  lua_Number (*version)(lua_State *L);
  void (*warning)(lua_State *L, const char *msg, int tocont);
  void (*where)(lua_State *L, int lvl);
  void (*xmove)(lua_State *from, lua_State *to, int n);
  void (*yieldk)(lua_State *L, int nresults, lua_KContext ctx, lua_KFunction k);
};

static const struct orbit_r_table *orbit_r;

size_t orbit_capi_table_size(void) {
  return sizeof(struct orbit_r_table);
}

/* ------------------------------------------------------------------------
 * States and the trampoline
 * --------------------------------------------------------------------- */

/* A state with LUA_EXTRASPACE bytes in front of it, as lua_getextraspace
   expects. */
lua_State *orbit_capi_newstate(void *rs, const struct orbit_r_table *table) {
  char *block = calloc(1, LUA_EXTRASPACE + sizeof(struct lua_State));
  lua_State *L;
  if (block == NULL) return NULL;
  L = (lua_State *)(block + LUA_EXTRASPACE);
  L->rs = rs;
  orbit_r = table;
  return L;
}

/* The state of another thread of main's VM. As in Lua, its extra space
   starts as a copy of the main thread's. */
lua_State *orbit_capi_newthread(lua_State *main) {
  char *block = malloc(LUA_EXTRASPACE + sizeof(struct lua_State));
  lua_State *L;
  if (block == NULL) return NULL;
  memcpy(block, lua_getextraspace(main), LUA_EXTRASPACE);
  L = (lua_State *)(block + LUA_EXTRASPACE);
  memset(L, 0, sizeof(*L));
  L->rs = main->rs;
  L->wake = main->wake;
  L->panicf = main->panicf;
  L->allocf = main->allocf;
  L->allocud = main->allocud;
  return L;
}

void orbit_capi_freestate(lua_State *L) {
  if (L != NULL) free((char *)L - LUA_EXTRASPACE);
}

/* Call f(L). Returns its result count, or -1 if it raised an error (the
   error value is pending on the Rust side). */
int orbit_capi_trampoline(lua_State *L, lua_CFunction f) {
  jmp_buf jb;
  jmp_buf *volatile old = L->jb;
  volatile int n = -1;
  L->jb = &jb;
  if (setjmp(jb) == 0) n = f(L);
  L->jb = old;
  return n;
}

/* Call a continuation k(L, status, ctx) as orbit_capi_trampoline calls a C
   function. */
int orbit_capi_trampoline_k(lua_State *L, lua_KFunction k, int status, lua_KContext ctx) {
  jmp_buf jb;
  jmp_buf *volatile old = L->jb;
  volatile int n = -1;
  L->jb = &jb;
  if (setjmp(jb) == 0) n = k(L, status, ctx);
  L->jb = old;
  return n;
}

static void orbit_throw(lua_State *L) {
  L->status = 0;
  if (L->jb == NULL && L->panicf != NULL) {
    /* An error outside every protected call: the panic function gets the
       error on the stack; if it returns, the program aborts. */
    orbit_r->pushpending(L);
    L->panicf(L);
    abort();
  }
  if (L->jb == NULL) {
    fputs("OrbitScript: Lua error raised outside a C function call\n", stderr);
    abort();
  }
  longjmp(*L->jb, 1);
}

#define CHECK(L)                                \
  do {                                          \
    if (luai_unlikely((L)->status)) orbit_throw(L); \
  } while (0)

/* An entry point returning a value, and one returning nothing. */
#define API(ret, name, params, args)                  \
  LUA_API ret lua_##name params {                     \
    ret r_ = orbit_r->name args;                     \
    CHECK(L);                                         \
    return r_;                                        \
  }
#define APIV(name, params, args)                      \
  LUA_API void lua_##name params {                    \
    orbit_r->name args;                              \
    CHECK(L);                                         \
  }

/* ------------------------------------------------------------------------
 * The core API, implemented in Rust
 * --------------------------------------------------------------------- */

API(lua_Number, version, (lua_State *L), (L))
API(int, absindex, (lua_State *L, int idx), (L, idx))
API(int, gettop, (lua_State *L), (L))
APIV(settop, (lua_State *L, int idx), (L, idx))
APIV(pushvalue, (lua_State *L, int idx), (L, idx))
APIV(rotate, (lua_State *L, int idx, int n), (L, idx, n))
APIV(copy, (lua_State *L, int fromidx, int toidx), (L, fromidx, toidx))
API(int, checkstack, (lua_State *L, int n), (L, n))

API(int, isnumber, (lua_State *L, int idx), (L, idx))
API(int, isstring, (lua_State *L, int idx), (L, idx))
API(int, iscfunction, (lua_State *L, int idx), (L, idx))
API(int, isinteger, (lua_State *L, int idx), (L, idx))
API(int, isuserdata, (lua_State *L, int idx), (L, idx))
API(int, type, (lua_State *L, int idx), (L, idx))
API(lua_Number, tonumberx, (lua_State *L, int idx, int *isnum), (L, idx, isnum))
API(lua_Integer, tointegerx, (lua_State *L, int idx, int *isnum), (L, idx, isnum))
API(int, toboolean, (lua_State *L, int idx), (L, idx))
API(const char *, tolstring, (lua_State *L, int idx, size_t *len), (L, idx, len))
API(lua_Unsigned, rawlen, (lua_State *L, int idx), (L, idx))
API(lua_CFunction, tocfunction, (lua_State *L, int idx), (L, idx))
API(void *, touserdata, (lua_State *L, int idx), (L, idx))
API(const void *, topointer, (lua_State *L, int idx), (L, idx))

API(int, rawequal, (lua_State *L, int a, int b), (L, a, b))
API(int, compare, (lua_State *L, int a, int b, int op), (L, a, b, op))

APIV(pushnil, (lua_State *L), (L))
APIV(pushnumber, (lua_State *L, lua_Number n), (L, n))
APIV(pushinteger, (lua_State *L, lua_Integer n), (L, n))
API(const char *, pushlstring, (lua_State *L, const char *s, size_t len), (L, s, len))
APIV(pushcclosure, (lua_State *L, lua_CFunction fn, int n), (L, fn, n))
APIV(pushboolean, (lua_State *L, int b), (L, b))
APIV(pushlightuserdata, (lua_State *L, void *p), (L, p))

API(int, getglobal, (lua_State *L, const char *name), (L, name))
API(int, gettable, (lua_State *L, int idx), (L, idx))
API(int, getfield, (lua_State *L, int idx, const char *k), (L, idx, k))
API(int, geti, (lua_State *L, int idx, lua_Integer n), (L, idx, n))
API(int, rawget, (lua_State *L, int idx), (L, idx))
API(int, rawgeti, (lua_State *L, int idx, lua_Integer n), (L, idx, n))
API(int, rawgetp, (lua_State *L, int idx, const void *p), (L, idx, p))
APIV(createtable, (lua_State *L, int narr, int nrec), (L, narr, nrec))
API(void *, newuserdatauv, (lua_State *L, size_t sz, int nuvalue), (L, sz, nuvalue))
API(int, getmetatable, (lua_State *L, int idx), (L, idx))
API(int, getiuservalue, (lua_State *L, int idx, int n), (L, idx, n))

APIV(setglobal, (lua_State *L, const char *name), (L, name))
APIV(settable, (lua_State *L, int idx), (L, idx))
APIV(setfield, (lua_State *L, int idx, const char *k), (L, idx, k))
APIV(seti, (lua_State *L, int idx, lua_Integer n), (L, idx, n))
APIV(rawset, (lua_State *L, int idx), (L, idx))
APIV(rawseti, (lua_State *L, int idx, lua_Integer n), (L, idx, n))
APIV(rawsetp, (lua_State *L, int idx, const void *p), (L, idx, p))
API(int, setmetatable, (lua_State *L, int idx), (L, idx))
API(int, setiuservalue, (lua_State *L, int idx, int n), (L, idx, n))

APIV(callk, (lua_State *L, int nargs, int nresults, lua_KContext ctx, lua_KFunction k),
     (L, nargs, nresults, ctx, k))
API(int, pcallk,
    (lua_State *L, int nargs, int nresults, int errfunc, lua_KContext ctx, lua_KFunction k),
    (L, nargs, nresults, errfunc, ctx, k))

API(int, error, (lua_State *L), (L))
API(int, next, (lua_State *L, int idx), (L, idx))
APIV(concat, (lua_State *L, int n), (L, n))
APIV(len, (lua_State *L, int idx), (L, idx))
API(size_t, stringtonumber, (lua_State *L, const char *s), (L, s))
APIV(arith, (lua_State *L, int op), (L, op))
API(unsigned, numbertocstring, (lua_State *L, int idx, char *buff), (L, idx, buff))
API(int, pushthread, (lua_State *L), (L))
API(int, getstack, (lua_State *L, int level, lua_Debug *ar), (L, level, ar))
API(int, getinfo, (lua_State *L, const char *what, lua_Debug *ar), (L, what, ar))
API(const char *, getupvalue, (lua_State *L, int funcindex, int n), (L, funcindex, n))
API(const char *, setupvalue, (lua_State *L, int funcindex, int n), (L, funcindex, n))
API(void *, upvalueid, (lua_State *L, int fidx, int n), (L, fidx, n))
APIV(upvaluejoin, (lua_State *L, int fidx1, int n1, int fidx2, int n2), (L, fidx1, n1, fidx2, n2))

LUA_API int lua_gc(lua_State *L, int what, ...) {
  va_list ap;
  int a = 0, b = 0, r;
  va_start(ap, what);
  if (what == LUA_GCSTEP) a = va_arg(ap, int);
  if (what == LUA_GCPARAM) {
    a = va_arg(ap, int);
    b = va_arg(ap, int);
  }
  va_end(ap);
  r = orbit_r->gc(L, what, a, b);
  CHECK(L);
  return r;
}

LUA_API const char *lua_typename(lua_State *L, int t) {
  static const char *const names[] = {"no value", "nil",   "boolean",  "userdata", "number",
                                      "string",   "table", "function", "userdata", "thread"};
  (void)L;
  if (t < LUA_TNONE || t >= LUA_NUMTYPES) return "?";
  return names[t + 1];
}

LUA_API const char *lua_pushstring(lua_State *L, const char *s) {
  if (s == NULL) {
    lua_pushnil(L);
    return NULL;
  }
  return lua_pushlstring(L, s, strlen(s));
}

/* The string is copied, so the caller's buffer is released at once. */
LUA_API const char *lua_pushexternalstring(lua_State *L, const char *s, size_t len,
                                           lua_Alloc falloc, void *ud) {
  const char *r = lua_pushlstring(L, s, len);
  if (falloc != NULL) falloc(ud, (void *)s, len + 1, 0);
  return r;
}

static void *orbit_default_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
  (void)ud;
  (void)osize;
  if (nsize == 0) {
    free(ptr);
    return NULL;
  }
  return realloc(ptr, nsize);
}

/* The allocator lua_newstate or lua_setallocf got is recorded but not
   used: memory comes from Rust's allocator (ADR-10). */
LUA_API lua_Alloc lua_getallocf(lua_State *L, void **ud) {
  if (L->allocf != NULL) {
    if (ud != NULL) *ud = L->allocud;
    return L->allocf;
  }
  if (ud != NULL) *ud = NULL;
  return orbit_default_alloc;
}

LUALIB_API void *luaL_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
  return orbit_default_alloc(ud, ptr, osize, nsize);
}

LUA_API lua_CFunction lua_atpanic(lua_State *L, lua_CFunction panicf) {
  lua_CFunction old = L->panicf;
  L->panicf = panicf;
  return old;
}

LUA_API void lua_warning(lua_State *L, const char *msg, int tocont) {
  orbit_r->warning(L, msg, tocont);
  CHECK(L);
}

/* ------------------------------------------------------------------------
 * lua_pushfstring: the manual's directives, %% %s %f %I %p %d %c %U
 * --------------------------------------------------------------------- */

typedef struct {
  char *p;
  size_t n, cap;
} FBuf;

static void fb_add(FBuf *b, const char *s, size_t l) {
  if (b->n + l > b->cap) {
    size_t cap = b->cap ? b->cap * 2 : 128;
    char *p;
    while (cap < b->n + l) cap *= 2;
    p = realloc(b->p, cap);
    if (p == NULL) abort();
    b->p = p;
    b->cap = cap;
  }
  memcpy(b->p + b->n, s, l);
  b->n += l;
}

static size_t utf8_encode(char *out, unsigned long x) {
  if (x < 0x80) {
    out[0] = (char)x;
    return 1;
  } else {
    char tmp[8];
    size_t n = 0, i;
    unsigned long limit = 0x3f;
    do {
      tmp[n++] = (char)(0x80 | (x & 0x3f));
      x >>= 6;
      limit >>= 1;
    } while (x > limit);
    tmp[n++] = (char)((~limit << 1) | x);
    for (i = 0; i < n; i++) out[i] = tmp[n - 1 - i];
    return n;
  }
}

LUA_API const char *lua_pushvfstring(lua_State *L, const char *fmt, va_list argp) {
  FBuf b = {NULL, 0, 0};
  const char *r;
  const char *e;
  char tmp[64];
  while ((e = strchr(fmt, '%')) != NULL) {
    fb_add(&b, fmt, (size_t)(e - fmt));
    switch (e[1]) {
      case 's': {
        const char *s = va_arg(argp, const char *);
        if (s == NULL) s = "(null)";
        fb_add(&b, s, strlen(s));
        break;
      }
      case 'c': {
        char c = (char)va_arg(argp, int);
        fb_add(&b, &c, 1);
        break;
      }
      case 'd': {
        int l = snprintf(tmp, sizeof tmp, "%d", va_arg(argp, int));
        fb_add(&b, tmp, (size_t)l);
        break;
      }
      case 'I': {
        int l = snprintf(tmp, sizeof tmp, "%lld", (long long)va_arg(argp, lua_Integer));
        fb_add(&b, tmp, (size_t)l);
        break;
      }
      case 'f': {
        /* Format through the VM so floats print as Lua prints them. */
        size_t l;
        const char *s;
        lua_pushnumber(L, (lua_Number)va_arg(argp, double));
        s = lua_tolstring(L, -1, &l);
        fb_add(&b, s, l);
        lua_settop(L, -2);
        break;
      }
      case 'p': {
        void *p = va_arg(argp, void *);
        int l = snprintf(tmp, sizeof tmp, "%p", p);
        fb_add(&b, tmp, (size_t)l);
        break;
      }
      case 'U': {
        size_t l = utf8_encode(tmp, (unsigned long)va_arg(argp, long));
        fb_add(&b, tmp, l);
        break;
      }
      case '%':
        fb_add(&b, "%", 1);
        break;
      default:
        free(b.p);
        luaL_error(L, "invalid option '%%%c' to 'lua_pushfstring'", e[1]);
    }
    fmt = e + 2;
  }
  fb_add(&b, fmt, strlen(fmt));
  r = lua_pushlstring(L, b.p, b.n);
  free(b.p);
  return r;
}

LUA_API const char *lua_pushfstring(lua_State *L, const char *fmt, ...) {
  const char *r;
  va_list ap;
  va_start(ap, fmt);
  r = lua_pushvfstring(L, fmt, ap);
  va_end(ap);
  return r;
}

/* ------------------------------------------------------------------------
 * Auxiliary library
 * --------------------------------------------------------------------- */


LUALIB_API int luaL_argerror(lua_State *L, int arg, const char *extramsg) {
  orbit_r->argerror(L, arg, extramsg);
  CHECK(L);
  return 0;
}

LUALIB_API int luaL_typeerror(lua_State *L, int arg, const char *tname) {
  orbit_r->typeerror(L, arg, tname);
  CHECK(L);
  return 0;
}

LUALIB_API void luaL_where(lua_State *L, int lvl) {
  orbit_r->where(L, lvl);
  CHECK(L);
}

LUALIB_API int luaL_error(lua_State *L, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  luaL_where(L, 1);
  lua_pushvfstring(L, fmt, ap);
  va_end(ap);
  lua_concat(L, 2);
  return lua_error(L);
}

LUALIB_API void luaL_checkversion_(lua_State *L, lua_Number ver, size_t sz) {
  if (sz != LUAL_NUMSIZES)
    luaL_error(L, "core and library have incompatible numeric types");
  else if (ver != LUA_VERSION_NUM)
    luaL_error(L, "version mismatch: app. needs %f, Lua core provides %f", (double)ver,
               (double)LUA_VERSION_NUM);
}

LUALIB_API int luaL_getmetafield(lua_State *L, int obj, const char *e) {
  int t;
  if (!lua_getmetatable(L, obj)) return LUA_TNIL;
  lua_pushstring(L, e);
  t = lua_rawget(L, -2);
  if (t == LUA_TNIL)
    lua_settop(L, -3);
  else
    lua_rotate(L, -2, -1), lua_settop(L, -2);
  return t;
}

LUALIB_API int luaL_callmeta(lua_State *L, int obj, const char *e) {
  obj = lua_absindex(L, obj);
  if (luaL_getmetafield(L, obj, e) == LUA_TNIL) return 0;
  lua_pushvalue(L, obj);
  lua_call(L, 1, 1);
  return 1;
}

LUALIB_API const char *luaL_tolstring(lua_State *L, int idx, size_t *len) {
  const char *r = orbit_r->tolstringmeta(L, idx, len);
  CHECK(L);
  return r;
}

static void tag_error(lua_State *L, int arg, int tag) {
  luaL_typeerror(L, arg, lua_typename(L, tag));
}

LUALIB_API const char *luaL_checklstring(lua_State *L, int arg, size_t *len) {
  const char *s = lua_tolstring(L, arg, len);
  if (s == NULL) tag_error(L, arg, LUA_TSTRING);
  return s;
}

LUALIB_API const char *luaL_optlstring(lua_State *L, int arg, const char *def, size_t *len) {
  if (lua_isnoneornil(L, arg)) {
    if (len != NULL) *len = def ? strlen(def) : 0;
    return def;
  }
  return luaL_checklstring(L, arg, len);
}

LUALIB_API lua_Number luaL_checknumber(lua_State *L, int arg) {
  int ok;
  lua_Number d = lua_tonumberx(L, arg, &ok);
  if (!ok) tag_error(L, arg, LUA_TNUMBER);
  return d;
}

LUALIB_API lua_Number luaL_optnumber(lua_State *L, int arg, lua_Number def) {
  return luaL_opt(L, luaL_checknumber, arg, def);
}

LUALIB_API lua_Integer luaL_checkinteger(lua_State *L, int arg) {
  int ok;
  lua_Integer d = lua_tointegerx(L, arg, &ok);
  if (!ok) {
    if (lua_isnumber(L, arg))
      luaL_argerror(L, arg, "number has no integer representation");
    else
      tag_error(L, arg, LUA_TNUMBER);
  }
  return d;
}

LUALIB_API lua_Integer luaL_optinteger(lua_State *L, int arg, lua_Integer def) {
  return luaL_opt(L, luaL_checkinteger, arg, def);
}

LUALIB_API void luaL_checkstack(lua_State *L, int sz, const char *msg) {
  if (!lua_checkstack(L, sz)) {
    if (msg)
      luaL_error(L, "stack overflow (%s)", msg);
    else
      luaL_error(L, "stack overflow");
  }
}

LUALIB_API void luaL_checktype(lua_State *L, int arg, int t) {
  if (lua_type(L, arg) != t) tag_error(L, arg, t);
}

LUALIB_API void luaL_checkany(lua_State *L, int arg) {
  if (lua_type(L, arg) == LUA_TNONE) luaL_argerror(L, arg, "value expected");
}

LUALIB_API int luaL_newmetatable(lua_State *L, const char *tname) {
  if (luaL_getmetatable(L, tname) != LUA_TNIL) return 0;
  lua_settop(L, -2);
  lua_createtable(L, 0, 2);
  lua_pushstring(L, tname);
  lua_setfield(L, -2, "__name");
  lua_pushvalue(L, -1);
  lua_setfield(L, LUA_REGISTRYINDEX, tname);
  return 1;
}

LUALIB_API void luaL_setmetatable(lua_State *L, const char *tname) {
  luaL_getmetatable(L, tname);
  lua_setmetatable(L, -2);
}

LUALIB_API void *luaL_testudata(lua_State *L, int ud, const char *tname) {
  void *p = lua_touserdata(L, ud);
  int same;
  if (p == NULL || !lua_getmetatable(L, ud)) return NULL;
  luaL_getmetatable(L, tname);
  same = lua_rawequal(L, -1, -2);
  lua_settop(L, -3);
  return same ? p : NULL;
}

LUALIB_API void *luaL_checkudata(lua_State *L, int ud, const char *tname) {
  void *p = luaL_testudata(L, ud, tname);
  if (p == NULL) {
    int rust = orbit_r->isrustudata(L, ud);
    CHECK(L);
    /* An OrbitScript library's userdata (an io file) has no C memory, so
       no C module can take it, whatever its type name says. */
    if (rust)
      luaL_argerror(L, ud,
                    lua_pushfstring(L, "%s expected, got a userdata OrbitScript made in Rust",
                                    tname));
    luaL_typeerror(L, ud, tname);
  }
  return p;
}

LUALIB_API int luaL_checkoption(lua_State *L, int arg, const char *def,
                                const char *const lst[]) {
  const char *name = def ? luaL_optstring(L, arg, def) : luaL_checkstring(L, arg);
  int i;
  for (i = 0; lst[i]; i++)
    if (strcmp(lst[i], name) == 0) return i;
  return luaL_argerror(L, arg, lua_pushfstring(L, "invalid option '%s'", name));
}

LUALIB_API int luaL_fileresult(lua_State *L, int stat, const char *fname) {
  int en = errno;
  if (stat) {
    lua_pushboolean(L, 1);
    return 1;
  }
  luaL_pushfail(L);
  if (fname)
    lua_pushfstring(L, "%s: %s", fname, strerror(en));
  else
    lua_pushstring(L, strerror(en));
  lua_pushinteger(L, en);
  return 3;
}

LUALIB_API int luaL_execresult(lua_State *L, int stat) {
  if (stat != 0 && errno != 0) return luaL_fileresult(L, 0, NULL);
  if (stat == 0)
    lua_pushboolean(L, 1);
  else
    luaL_pushfail(L);
  lua_pushstring(L, "exit");
  lua_pushinteger(L, stat);
  return 3;
}

/* References: the free list's head lives under a key only this file can
   make, a light userdata for a static address. */
static const char freelist_key = 0;

LUALIB_API int luaL_ref(lua_State *L, int t) {
  lua_Integer ref;
  if (lua_isnil(L, -1)) {
    lua_settop(L, -2);
    return LUA_REFNIL;
  }
  t = lua_absindex(L, t);
  if (lua_rawgetp(L, t, &freelist_key) == LUA_TNUMBER && lua_tointeger(L, -1) != 0) {
    ref = lua_tointeger(L, -1);
    lua_settop(L, -2);
    lua_rawgeti(L, t, ref);
    lua_rawsetp(L, t, &freelist_key);
  } else {
    lua_settop(L, -2);
    ref = (lua_Integer)lua_rawlen(L, t) + 1;
  }
  lua_rawseti(L, t, ref);
  return (int)ref;
}

LUALIB_API void luaL_unref(lua_State *L, int t, int ref) {
  if (ref < 0) return;
  t = lua_absindex(L, t);
  lua_rawgetp(L, t, &freelist_key);
  if (lua_isnil(L, -1)) {
    lua_settop(L, -2);
    lua_pushinteger(L, 0);
  }
  lua_rawseti(L, t, ref);
  lua_pushinteger(L, ref);
  lua_rawsetp(L, t, &freelist_key);
}

LUALIB_API lua_Integer luaL_len(lua_State *L, int idx) {
  int ok;
  lua_Integer l;
  lua_len(L, idx);
  l = lua_tointegerx(L, -1, &ok);
  if (!ok) luaL_error(L, "object length is not an integer");
  lua_settop(L, -2);
  return l;
}

LUALIB_API void luaL_setfuncs(lua_State *L, const luaL_Reg *l, int nup) {
  luaL_checkstack(L, nup, "too many upvalues");
  for (; l->name != NULL; l++) {
    if (l->func == NULL) {
      lua_pushboolean(L, 0);
    } else {
      int i;
      for (i = 0; i < nup; i++) lua_pushvalue(L, -nup);
      lua_pushcclosure(L, l->func, nup);
    }
    lua_setfield(L, -(nup + 2), l->name);
  }
  lua_settop(L, -nup - 1);
}

LUALIB_API int luaL_getsubtable(lua_State *L, int idx, const char *fname) {
  if (lua_getfield(L, idx, fname) == LUA_TTABLE) return 1;
  lua_settop(L, -2);
  idx = lua_absindex(L, idx);
  lua_createtable(L, 0, 0);
  lua_pushvalue(L, -1);
  lua_setfield(L, idx, fname);
  return 0;
}

LUALIB_API void luaL_requiref(lua_State *L, const char *modname, lua_CFunction openf,
                              int glb) {
  luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
  lua_getfield(L, -1, modname);
  if (!lua_toboolean(L, -1)) {
    lua_settop(L, -2);
    lua_pushcclosure(L, openf, 0);
    lua_pushstring(L, modname);
    lua_call(L, 1, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, -3, modname);
  }
  lua_rotate(L, -2, -1);
  lua_settop(L, -2);
  if (glb) {
    lua_pushvalue(L, -1);
    lua_setglobal(L, modname);
  }
}

LUALIB_API const char *luaL_gsub(lua_State *L, const char *s, const char *p, const char *r) {
  luaL_Buffer b;
  luaL_buffinit(L, &b);
  luaL_addgsub(&b, s, p, r);
  luaL_pushresult(&b);
  return lua_tostring(L, -1);
}

LUALIB_API void luaL_addgsub(luaL_Buffer *b, const char *s, const char *p, const char *r) {
  const char *wild;
  size_t l = strlen(p);
  while ((wild = strstr(s, p)) != NULL) {
    luaL_addlstring(b, s, (size_t)(wild - s));
    luaL_addstring(b, r);
    s = wild + l;
  }
  luaL_addstring(b, s);
}

/* Buffers. While a buffer is in use, its stack slot is the top one (the one
   below the top in luaL_addvalue). It holds nil until the buffer outgrows
   its inline storage, then a userdata box with the contents. */

static char *buff_grow(luaL_Buffer *B, size_t extra, int slot) {
  lua_State *L = B->L;
  size_t need = B->n + extra, cap = B->size * 2;
  char *box;
  if (need < B->n) luaL_error(L, "buffer too large");
  if (cap < need) cap = need;
  box = (char *)lua_newuserdatauv(L, cap, 0);
  memcpy(box, B->b, B->n);
  lua_copy(L, -1, slot);
  lua_settop(L, -2);
  B->b = box;
  B->size = cap;
  return box;
}

LUALIB_API void luaL_buffinit(lua_State *L, luaL_Buffer *B) {
  B->L = L;
  B->b = B->init.b;
  B->n = 0;
  B->size = LUAL_BUFFERSIZE;
  lua_pushnil(L);
}

LUALIB_API char *luaL_prepbuffsize(luaL_Buffer *B, size_t sz) {
  if (B->size - B->n < sz) buff_grow(B, sz, lua_absindex(B->L, -1));
  return B->b + B->n;
}

LUALIB_API void luaL_addlstring(luaL_Buffer *B, const char *s, size_t l) {
  if (l > 0) {
    memcpy(luaL_prepbuffsize(B, l), s, l);
    B->n += l;
  }
}

LUALIB_API void luaL_addstring(luaL_Buffer *B, const char *s) {
  luaL_addlstring(B, s, strlen(s));
}

LUALIB_API void luaL_addvalue(luaL_Buffer *B) {
  lua_State *L = B->L;
  size_t l;
  const char *s = lua_tolstring(L, -1, &l);
  if (B->size - B->n < l) buff_grow(B, l, lua_absindex(L, -2));
  memcpy(B->b + B->n, s, l);
  B->n += l;
  lua_settop(L, -2);
}

LUALIB_API void luaL_pushresult(luaL_Buffer *B) {
  lua_State *L = B->L;
  lua_pushlstring(L, B->b, B->n);
  lua_rotate(L, -2, -1);
  lua_settop(L, -2);
}

LUALIB_API void luaL_pushresultsize(luaL_Buffer *B, size_t sz) {
  B->n += sz;
  luaL_pushresult(B);
}

LUALIB_API char *luaL_buffinitsize(lua_State *L, luaL_Buffer *B, size_t sz) {
  luaL_buffinit(L, B);
  return luaL_prepbuffsize(B, sz);
}

/* ------------------------------------------------------------------------
 * Loading chunks
 * --------------------------------------------------------------------- */


/* The buffer is freed before any error is raised, so no jump leaks it. */
static int load_bytes(lua_State *L, char *buf, size_t len, const char *chunkname,
                      const char *mode, int owned) {
  int status = orbit_r->load(L, buf, len, chunkname, mode);
  if (owned) free(buf);
  CHECK(L);
  return status;
}

LUA_API int lua_load(lua_State *L, lua_Reader reader, void *data, const char *chunkname,
                     const char *mode) {
  char *buf = NULL;
  size_t len = 0, cap = 0, n;
  const char *piece;
  while ((piece = reader(L, data, &n)) != NULL && n > 0) {
    if (len + n > cap) {
      char *p;
      size_t ncap = cap ? cap * 2 : 1024;
      while (ncap < len + n) ncap *= 2;
      p = realloc(buf, ncap);
      if (p == NULL) {
        free(buf);
        lua_pushliteral(L, "not enough memory");
        return LUA_ERRMEM;
      }
      buf = p;
      cap = ncap;
    }
    memcpy(buf + len, piece, n);
    len += n;
  }
  return load_bytes(L, buf, len, chunkname ? chunkname : "?", mode, 1);
}

LUALIB_API int luaL_loadbufferx(lua_State *L, const char *buff, size_t sz, const char *name,
                                const char *mode) {
  return load_bytes(L, (char *)buff, sz, name, mode, 0);
}

LUALIB_API int luaL_loadstring(lua_State *L, const char *s) {
  return luaL_loadbufferx(L, s, strlen(s), s, NULL);
}

LUALIB_API int luaL_loadfilex(lua_State *L, const char *filename, const char *mode) {
  FILE *f;
  char *buf = NULL, *p;
  size_t len = 0, cap = 0, n;
  int status;
  const char *chunkname;
  int top = lua_gettop(L);
  if (filename == NULL) {
    lua_pushliteral(L, "=stdin");
    f = stdin;
  } else {
    lua_pushfstring(L, "@%s", filename);
    f = fopen(filename, "rb");
    if (f == NULL) {
      int en = errno;
      lua_pushfstring(L, "cannot open %s: %s", filename, strerror(en));
      lua_rotate(L, -2, -1);
      lua_settop(L, -2);
      return LUA_ERRFILE;
    }
  }
  for (;;) {
    if (len == cap) {
      cap = cap ? cap * 2 : 4096;
      p = realloc(buf, cap);
      if (p == NULL) {
        free(buf);
        if (f != stdin) fclose(f);
        lua_settop(L, top);
        lua_pushliteral(L, "not enough memory");
        return LUA_ERRMEM;
      }
      buf = p;
    }
    n = fread(buf + len, 1, cap - len, f);
    len += n;
    if (n == 0) break;
  }
  if (ferror(f)) {
    int en = errno;
    free(buf);
    if (f != stdin) fclose(f);
    lua_settop(L, top);
    lua_pushfstring(L, "cannot read %s: %s", filename ? filename : "stdin", strerror(en));
    return LUA_ERRFILE;
  }
  if (f != stdin) fclose(f);
  /* A first line starting with '#' (a Unix exec line) is skipped, keeping
     its newline so line numbers stay right. */
  p = buf;
  /* A UTF-8 byte order mark is skipped too. */
  if (len >= 3 && memcmp(p, "\xEF\xBB\xBF", 3) == 0) p += 3;
  if (p < buf + len && *p == '#') {
    while (p < buf + len && *p != '\n') p++;
    /* A binary chunk after the line starts right after its newline. */
    if (p + 1 < buf + len && p[1] == LUA_SIGNATURE[0]) p++;
  }
  chunkname = lua_tostring(L, -1);
  status = orbit_r->load(L, p, len - (size_t)(p - buf), chunkname, mode);
  free(buf);
  CHECK(L);
  lua_rotate(L, -2, -1); /* drop the chunk name */
  lua_settop(L, -2);
  return status;
}

/* ------------------------------------------------------------------------
 * Tracebacks, dumps and the rest of the API
 * --------------------------------------------------------------------- */


static void unsupported(lua_State *L, const char *what) {
  orbit_r->unsupported(L, what);
  CHECK(L);
}

/* Only this state's own thread has a lua_State. */
LUALIB_API void luaL_traceback(lua_State *L, lua_State *L1, const char *msg, int level) {
  if (L1 != L) unsupported(L, "luaL_traceback of another thread");
  orbit_r->traceback(L, msg, level);
  CHECK(L);
}

LUA_API int lua_dump(lua_State *L, lua_Writer writer, void *data, int strip) {
  /* Through string.dump, which writes OrbitScript's own binary chunks. */
  int top = lua_gettop(L), status = 1;
  size_t len;
  const char *s;
  luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_LOADED_TABLE);
  if (lua_getfield(L, -1, "string") == LUA_TTABLE && lua_getfield(L, -1, "dump") == LUA_TFUNCTION) {
    lua_pushvalue(L, top);
    lua_pushboolean(L, strip);
    if (lua_pcall(L, 2, 1, 0) == LUA_OK && (s = lua_tolstring(L, -1, &len)) != NULL)
      status = writer(L, s, len, data);
  }
  lua_settop(L, top);
  return status;
}

/* Threads. Every thread C sees has its own lua_State (made in Rust with
   orbit_capi_newthread). lua_yieldk, and lua_callk/lua_pcallk with a
   continuation where the thread can yield, leave the C function with a
   longjmp, as an error does: Rust then does what it asked for. */
API(int, status, (lua_State *L), (L))
API(int, isyieldable, (lua_State *L), (L))
API(lua_State *, tothread, (lua_State *L, int idx), (L, idx))
API(lua_State *, newthread, (lua_State *L), (L))
API(int, closethread, (lua_State *L, lua_State *from), (L, from))
API(int, resume, (lua_State *L, lua_State *from, int narg, int *nres), (L, from, narg, nres))

LUA_API void lua_xmove(lua_State *from, lua_State *to, int n) {
  orbit_r->xmove(from, to, n);
  CHECK(from);
}

LUA_API int lua_yieldk(lua_State *L, int nresults, lua_KContext ctx, lua_KFunction k) {
  orbit_r->yieldk(L, nresults, ctx, k);
  CHECK(L);
  return 0;
}

APIV(toclose, (lua_State *L, int idx), (L, idx))
APIV(closeslot, (lua_State *L, int idx), (L, idx))

API(const char *, getlocal, (lua_State *L, const lua_Debug *ar, int n), (L, ar, n))
API(const char *, setlocal, (lua_State *L, const lua_Debug *ar, int n), (L, ar, n))

/* Safe to call from a signal handler, as Lua's own interpreter does on
   Ctrl-C: it only stores the request and raises the VM's interrupt flag.
   The VM installs the hook at its next check (a loop's backward jump, a
   hook point, or the next API call). */
LUA_API void lua_sethook(lua_State *L, lua_Hook func, int mask, int count) {
  if (func == NULL || mask == 0) {
    func = NULL;
    mask = 0;
  }
  store_hook(&L->hook, func);
  store_int(&L->hookmask, mask);
  store_int(&L->hookcount, count);
  store_int_release(&L->hookchanged, 1);
  if (L->wake != NULL) store_byte(L->wake, 1);
}

LUA_API lua_Hook lua_gethook(lua_State *L) {
  return load_hook(&L->hook);
}

LUA_API int lua_gethookmask(lua_State *L) {
  return load_int(&L->hookmask);
}

LUA_API int lua_gethookcount(lua_State *L) {
  return load_int(&L->hookcount);
}

/* For Rust: where lua_sethook raises the interrupt flag. */
void orbit_capi_set_wake(lua_State *L, unsigned char *wake) {
  L->wake = wake;
}

/* For Rust: the hook lua_sethook asked for, if it asked since the last
   call (1), or 0. */
int orbit_capi_take_hook(lua_State *L, lua_Hook *f, int *mask, int *count) {
  if (!exchange_int_acquire(&L->hookchanged, 0)) return 0;
  *f = load_hook(&L->hook);
  *mask = load_int(&L->hookmask);
  *count = load_int(&L->hookcount);
  return 1;
}

/* For Rust: run hook f for an event, as orbit_capi_trampoline runs a C
   function. `level` is what lua_getinfo needs to find the hooked function.
   Returns 0, or -1 if the hook raised an error (pending on the Rust side). */
int orbit_capi_call_hook(lua_State *L, lua_Hook f, int event, int line, size_t level) {
  jmp_buf jb;
  jmp_buf *volatile old = L->jb;
  volatile int r = -1;
  lua_Debug ar;
  memset(&ar, 0, sizeof ar);
  ar.event = event;
  ar.currentline = line;
  ar.orbit_private = (void *)level;
  L->jb = &jb;
  if (setjmp(jb) == 0) {
    f(L, &ar);
    r = 0;
  }
  L->jb = old;
  return r;
}

/* ------------------------------------------------------------------------
 * States created from C (ADR-10) and the standard libraries
 * --------------------------------------------------------------------- */

const char lua_ident[] = "$LuaVersion: " LUA_COPYRIGHT " $";

/* print's output in a state C created goes through C's stdout. */
size_t orbit_capi_write_stdout(const char *p, size_t n) {
  return fwrite(p, 1, n, stdout);
}

int orbit_capi_flush_stdout(void) {
  return fflush(stdout);
}

static int orbit_openlib(lua_State *L, int lib) {
  int n = orbit_r->openlib(L, lib);
  CHECK(L);
  if (n == 0) return luaL_error(L, "library not included in this build of OrbitScript");
  return n;
}

/* Indices are the libraries' bit positions in lualib.h's masks. */
LUAMOD_API int luaopen_base(lua_State *L) { return orbit_openlib(L, 0); }
LUAMOD_API int luaopen_package(lua_State *L) { return orbit_openlib(L, 1); }
LUAMOD_API int luaopen_coroutine(lua_State *L) { return orbit_openlib(L, 2); }
LUAMOD_API int luaopen_debug(lua_State *L) { return orbit_openlib(L, 3); }
LUAMOD_API int luaopen_io(lua_State *L) { return orbit_openlib(L, 4); }
LUAMOD_API int luaopen_math(lua_State *L) { return orbit_openlib(L, 5); }
LUAMOD_API int luaopen_os(lua_State *L) { return orbit_openlib(L, 6); }
LUAMOD_API int luaopen_string(lua_State *L) { return orbit_openlib(L, 7); }
LUAMOD_API int luaopen_table(lua_State *L) { return orbit_openlib(L, 8); }
LUAMOD_API int luaopen_utf8(lua_State *L) { return orbit_openlib(L, 9); }

static const struct {
  const char *name;
  lua_CFunction open;
} orbit_libs[] = {
    {LUA_GNAME, luaopen_base},          {LUA_LOADLIBNAME, luaopen_package},
    {LUA_COLIBNAME, luaopen_coroutine}, {LUA_DBLIBNAME, luaopen_debug},
    {LUA_IOLIBNAME, luaopen_io},        {LUA_MATHLIBNAME, luaopen_math},
    {LUA_OSLIBNAME, luaopen_os},        {LUA_STRLIBNAME, luaopen_string},
    {LUA_TABLIBNAME, luaopen_table},    {LUA_UTF8LIBNAME, luaopen_utf8},
};

LUALIB_API void luaL_openselectedlibs(lua_State *L, int load, int preload) {
  int i;
  luaL_getsubtable(L, LUA_REGISTRYINDEX, LUA_PRELOAD_TABLE);
  for (i = 0; i < (int)(sizeof(orbit_libs) / sizeof(orbit_libs[0])); i++) {
    int mask = 1 << i;
    if (load & mask) {
      luaL_requiref(L, orbit_libs[i].name, orbit_libs[i].open, 1);
      lua_pop(L, 1);
    } else if (preload & mask) {
      lua_pushcfunction(L, orbit_libs[i].open);
      lua_setfield(L, -2, orbit_libs[i].name);
    }
  }
  lua_pop(L, 1);
}

extern int orbit_r_setlocalemode(lua_State *L, int mode);

LUA_API int orbit_setlocalemode(lua_State *L, int mode) {
  return orbit_r_setlocalemode(L, mode);
}

#if defined(ORBIT_CAPI_HOST)
extern lua_State *orbit_r_newstate(unsigned flags);
extern int orbit_r_debug_listen(lua_State *L, int announce);
extern void orbit_r_debug_stop(void);

static void orbit_debug_atexit(void) { orbit_r_debug_stop(); }

/* Listen for debuggers, saying where with `announce`; the socket goes
   away at exit. */
static int orbit_listen(lua_State *L, int announce) {
  int r = orbit_r_debug_listen(L, announce);
  if (r == 1) atexit(orbit_debug_atexit);
  return r < 0 ? -1 : 0;
}

LUA_API int orbit_debug_listen(lua_State *L, const char *addr) {
  if (addr != NULL) return -1;
  return orbit_listen(L, 0);
}

/* A new state, debuggable unless `flags` says not, listening at once
   when the environment asks (ORBIT_DEBUG=1). */
static lua_State *orbit_makestate(unsigned flags) {
  lua_State *L = orbit_r_newstate(flags);
  const char *debug;
  if (L == NULL) return NULL;
  debug = getenv("ORBIT_DEBUG");
  if (!(flags & ORBIT_STATE_NO_DEBUGGER) && debug != NULL && strcmp(debug, "1") == 0)
    orbit_listen(L, 1);
  return L;
}

LUA_API lua_State *orbit_newstate(lua_Alloc f, void *ud, unsigned flags) {
  lua_State *L = orbit_makestate(flags);
  if (L == NULL) return NULL;
  L->allocf = f;
  L->allocud = ud;
  /* A plain state has no warning function: warnings are dropped. */
  lua_setwarnf(L, NULL, NULL);
  return L;
}

LUA_API lua_State *lua_newstate(lua_Alloc f, void *ud, unsigned seed) {
  (void)seed;
  return orbit_newstate(f, ud, 0);
}

static int orbit_panic(lua_State *L) {
  const char *msg = lua_type(L, -1) == LUA_TSTRING ? lua_tostring(L, -1)
                                                   : "error object is not a string";
  fprintf(stderr, "PANIC: unprotected error in call to Lua API (%s)\n", msg);
  fflush(stderr);
  return 0;
}

LUALIB_API lua_State *luaL_newstate(void) {
  lua_State *L = orbit_makestate(0);
  if (L != NULL) lua_atpanic(L, orbit_panic);
  return L;
}

LUA_API void lua_close(lua_State *L) {
  orbit_r->close(L);
}
#else
/* Without the `host` feature only Rust creates states. */
LUA_API lua_State *lua_newstate(lua_Alloc f, void *ud, unsigned seed) {
  (void)f;
  (void)ud;
  (void)seed;
  return NULL;
}

LUALIB_API lua_State *luaL_newstate(void) {
  return NULL;
}

LUA_API lua_State *orbit_newstate(lua_Alloc f, void *ud, unsigned flags) {
  (void)f;
  (void)ud;
  (void)flags;
  return NULL;
}

LUA_API int orbit_debug_listen(lua_State *L, const char *addr) {
  (void)L;
  (void)addr;
  return -1;
}

LUA_API void lua_close(lua_State *L) {
  (void)L;
}
#endif

LUA_API void lua_setallocf(lua_State *L, lua_Alloc f, void *ud) {
  L->allocf = f;
  L->allocud = ud;
}

LUA_API void lua_setwarnf(lua_State *L, lua_WarnFunction f, void *ud) {
  orbit_r->setwarnf(L, f, ud);
  CHECK(L);
}

LUALIB_API unsigned luaL_makeseed(lua_State *L) {
  static int addr;
  (void)L;
  return (unsigned)(size_t)&addr ^ (unsigned)time(NULL);
}
