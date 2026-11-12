#!/usr/bin/env sh
# Builds a reference Lua interpreter and the test suite's C libraries.
#
#   scripts/build-reference-lua.sh [OUT_DIR]      (default: target/reference-lua)
#   LUA_VERSION=5.4 scripts/build-reference-lua.sh  (default: target/reference-lua-5.4)
#
# LUA_VERSION (5.5 or 5.4, default 5.5) picks the Lua the build targets
# (ADR-14). 5.4 is built with LUA_COMPAT_5_3, as Debian builds it.
#
# Produces OUT_DIR/bin/luaX.Y and OUT_DIR/libs/*.so. On Linux it also builds
# the reference library as Debian packages it (OUT_DIR/shared/libluaX.Y.so.0,
# symbols versioned LUA_X.Y) and OUT_DIR/bin/luaX.Y-host, the reference
# `lua` program linked to it, which `cargo testkit liblua` runs on
# OrbitScript's liborbit instead (ADR-10). The source is the
# GitHub mirror of the Lua repository (www.lua.org is not reachable from
# every sandbox) at the tag the vendored suite came from, as recorded in
# tests/lua-X.Y/VERSION. Needs git and a C compiler. Linux and macOS only.
# Running the suite's main.lua with it also needs the readline library under
# its unversioned name (libreadline.so, from libreadline-dev on Debian and
# Ubuntu): luaX.Y -i loads it, and the test expects no warning.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
LUA_VERSION=${LUA_VERSION:-5.5}
case "$LUA_VERSION" in
    5.5) default_out="$root/target/reference-lua"; compat="" ;;
    5.4) default_out="$root/target/reference-lua-5.4"; compat=-DLUA_COMPAT_5_3 ;;
    *) echo "unsupported LUA_VERSION: $LUA_VERSION (5.5 or 5.4)" >&2; exit 1 ;;
esac
V=$LUA_VERSION
LUA_TAG=${LUA_TAG:-$(cut -d' ' -f1 "$root/tests/lua-$V/VERSION")}
LUA_REPO=${LUA_REPO:-https://github.com/lua/lua}
CC=${CC:-cc}
out=${1:-"$default_out"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
src="$out/src"

if [ ! -f "$src/onelua.c" ] || [ "$(cat "$out/TAG" 2>/dev/null)" != "$LUA_TAG" ]; then
    rm -rf "$src"
    git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$LUA_TAG" "$LUA_REPO" "$src"
    echo "$LUA_TAG" > "$out/TAG"
fi

case "$(uname -s)" in
    Linux)  sysdef=-DLUA_USE_LINUX;  syslibs="-lm -ldl"
            # 5.5's lua loads libreadline at run time; 5.4's links it, as
            # Debian builds it (main.lua's -i checks expect it).
            if [ "$V" = 5.4 ]; then sysdef="$sysdef -DLUA_USE_READLINE"; syslibs="$syslibs -lreadline"; fi
            export_flag=-Wl,-E; shared="-shared -fPIC" ;;
    Darwin) sysdef=-DLUA_USE_MACOSX; syslibs="-lm -lreadline"; export_flag=""; shared="-bundle -undefined dynamic_lookup -fPIC" ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

mkdir -p "$out/bin" "$out/libs"
# onelua.c is the whole interpreter as one translation unit; it matches a
# release build of lua.c without the internal test library (ltests.c).
# shellcheck disable=SC2086
$CC -std=gnu99 -O2 $sysdef $compat -o "$out/bin/lua$V" "$src/onelua.c" $export_flag $syslibs

# C modules loaded by attrib.lua and main.lua, as the suite's libs/makefile builds them.
for lib in lib1 lib11 lib2 lib21; do
    # shellcheck disable=SC2086
    $CC -O2 -Wall -I"$src" $shared -o "$out/libs/$lib.so" "$src/testes/libs/$lib.c"
done
# shellcheck disable=SC2086
$CC -O2 -Wall -I"$src" $shared -o "$out/libs/lib2-v2.so" "$src/testes/libs/lib22.c"

# The reference library as Debian's libluaX.Y-0 ships it, and lua.c linked
# to it: a program built for liblua, without OrbitScript in mind.
if [ "$(uname -s)" = Linux ]; then
    mkdir -p "$out/shared"
    printf 'LUA_%s { global: lua_*; luaL_*; luaopen_*; local: *; };\n' "$V" > "$out/shared/lua.map"
    libsrc=$(ls "$src"/l*.c | grep -v -e '/lua\.c$' -e '/luac\.c$' -e '/ltests\.c$')
    # shellcheck disable=SC2086
    $CC -std=gnu99 -O2 $sysdef $compat -fPIC -shared -o "$out/shared/liblua$V.so.0" \
        -Wl,-soname,liblua$V.so.0 -Wl,--version-script="$out/shared/lua.map" $libsrc -lm -ldl
    # shellcheck disable=SC2086
    $CC -std=gnu99 -O2 $sysdef $compat -o "$out/bin/lua$V-host" "$src/lua.c" \
        "$out/shared/liblua$V.so.0" -Wl,-rpath,'$ORIGIN/../shared' $syslibs
fi

"$out/bin/lua$V" -v
