#!/usr/bin/env sh
# Builds OrbitScript's shared layout (ADR-10): liborbit holds the runtime
# once, `orbit` and `orbitc` are built with `orbit`'s `shared` feature and
# find it through their run path, and Lua 5.5's library names are aliases
# of it, so programs built for Lua 5.5 use it too.
#
#   ./build.sh [--out DIR] [--profile NAME]
#
#   --out DIR       where to lay it out (default target/package/out)
#   --profile NAME  the cargo profile (default release)
#
# Layout:
#   Linux:   bin/orbit, bin/orbitc
#            lib/liborbit.so.0, and liborbit.so, liblua5.5.so.0 (Debian),
#                liblua-5.5.so (Fedora), liblua.so.5.5 (upstream) linking to it
#   macOS:   bin/orbit, bin/orbitc
#            lib/liborbit.0.dylib, and liborbit.dylib, liblua.5.5.dylib,
#                liblua5.5.dylib linking to it
#   Windows: bin/orbit.exe, orbitc.exe, orbit_lib.dll, and lua55.dll, which
#                forwards Lua's names to orbit_lib.dll (a copy would be a
#                second runtime in the process); run from Git Bash or MSYS2
#   all:     include/lua.h, luaconf.h, lualib.h, lauxlib.h
#
# The builds use their own target directory (target/package), so the static
# target/release/orbit is left alone. The default `cargo build` stays the
# static, one-file `orbit`.
set -eu

root=$(cd "$(dirname "$0")" && pwd)
out="$root/target/package/out"
profile=release
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out=${2:?--out needs a value}; shift 2 ;;
        --profile) profile=${2:?--profile needs a value}; shift 2 ;;
        -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "build.sh: unknown option $1" >&2; exit 2 ;;
    esac
done

target="$root/target/package"
cargo=${CARGO:-cargo}
cd "$root"
# The library first: the programs link against it.
CARGO_TARGET_DIR="$target" "$cargo" build --profile "$profile" -p orbit-lib --features debugger
CARGO_TARGET_DIR="$target" "$cargo" build --profile "$profile" -p orbit-cli \
    --no-default-features --features shared,line-editing,dap
case "$profile" in
    dev) built="$target/debug" ;;
    *) built="$target/$profile" ;;
esac

rm -rf "$out"
mkdir -p "$out/bin" "$out/lib" "$out/include"
out=$(cd "$out" && pwd)
headers="$root/crates/orbit-capi/csrc/include"

# The function or variable each LUA_API, LUALIB_API and LUAMOD_API
# declaration in our headers names, one per line.
api_names() {
    grep -hE '^(LUA_API|LUALIB_API|LUAMOD_API) ' "$headers/lua.h" "$headers/lauxlib.h" "$headers/lualib.h" |
        awk '{ if (match($0, /(lua|luaL|luaopen)_[a-z][A-Za-z0-9_]*/)) print substr($0, RSTART, RLENGTH) }' |
        sort -u
}

case "$(uname -s)" in
    Darwin)
        cp "$built/orbit" "$built/orbitc" "$out/bin/"
        lib=liborbit.0.dylib
        cp "$built/liborbit_lib.dylib" "$out/lib/$lib"
        for alias in liborbit.dylib liblua.5.5.dylib liblua5.5.dylib; do
            ln -s "$lib" "$out/lib/$alias"
        done
        files="bin/orbit bin/orbitc lib/$lib"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        for f in orbit.exe orbitc.exe orbit_lib.dll; do
            cp "$built/$f" "$out/bin/$f"
        done
        def="$target/lua55.def"
        {
            echo "LIBRARY lua55.dll"
            echo "EXPORTS"
            api_names | sed 's/.*/  & = orbit_lib.&/'
        } > "$def"
        # A DLL with no code, only forwarded exports, linked with the
        # toolchain's linker: MSVC's `link` where it is on the path, else
        # MinGW's gcc.
        if command -v link.exe > /dev/null 2>&1 && link.exe /? 2>&1 | grep -q Microsoft; then
            link.exe /NOLOGO /DLL /NOENTRY "/DEF:$(cygpath -w "$def")" "/OUT:$(cygpath -w "$out/bin/lua55.dll")"
        else
            "${CC:-gcc}" -shared -nostdlib -o "$out/bin/lua55.dll" "$def"
        fi
        files="bin/orbit.exe bin/orbitc.exe bin/orbit_lib.dll bin/lua55.dll"
        ;;
    *)
        cp "$built/orbit" "$built/orbitc" "$out/bin/"
        lib=liborbit.so.0
        cp "$built/liborbit_lib.so" "$out/lib/$lib"
        for alias in liborbit.so liblua5.5.so.0 liblua-5.5.so liblua.so.5.5; do
            ln -s "$lib" "$out/lib/$alias"
        done
        files="bin/orbit bin/orbitc lib/$lib"
        ;;
esac
for h in lua.h luaconf.h lualib.h lauxlib.h; do
    cp "$headers/$h" "$out/include/$h"
done

total=0
for f in $files; do
    n=$(wc -c < "$out/$f" | tr -d ' ')
    total=$((total + n))
    printf '%10s  %s\n' "$n" "$out/$f"
done
printf '%10s  total\n' "$total"
