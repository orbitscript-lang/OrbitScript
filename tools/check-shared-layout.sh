#!/usr/bin/env sh
# Builds the shared layout (ADR-10) and tests it as it is laid out: the
# programs with liborbit beside them, not Cargo's own static builds.
#
#   tools/check-shared-layout.sh [build.sh options]
#
# The options go to ./build.sh: --lua 5.4, and --pgo FILE or --pgo-train;
# without those two the build has no profile, which is enough for the
# tests. The tests are those of the default features, so the options that
# leave parts out (--no-ffi, --no-debugger) are not taken. The layout is
# left in target/package/out (a 5.4 build's in target/package-lua5.4/out).
#
# 1. The library swap. A second build of the same sources stands in for
#    another build (ORBIT_BUILD_SALT changes only its identity). Its liborbit
#    has every symbol the programs look for, so the loader accepts it; the
#    programs must refuse it themselves, by name, when they start.
# 2. The tests of `orbit`, `orbitc` and `orbit-lsp`, run on the laid-out
#    programs (ORBIT_BIN_DIR).
# 3. On Linux, the C host tests on the laid-out liborbit (ORBIT_LIBORBIT).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

lua=5.5
pgo=--no-pgo
prev=
for a in "$@"; do
    case "$a" in
        --pgo | --pgo-train) pgo= ;;
        --out | --static | --pgo-check | --no-pgo | --no-ffi | --no-debugger | --system-libffi | --features)
            echo "check-shared-layout.sh: $a is not for this script" >&2
            exit 2
            ;;
    esac
    [ "$prev" != --lua ] || lua=$a
    prev=$a
done
case "$lua" in
    5.5) package=package test_features= ;;
    5.4) package=package-lua5.4 test_features="--features orbit/lua-5-4" ;;
    *) echo "check-shared-layout.sh: --lua must be 5.5 or 5.4, not $lua" >&2; exit 2 ;;
esac
out="$root/target/$package/out"
other="$root/target/$package/other-build"

case "$(rustc -vV | sed -n 's/^host: //p')" in
    *-windows-*) library=bin/orbit_lib.dll exe=.exe ;;
    *-apple-*) library=lib/liborbit.0.dylib exe= ;;
    *) library=lib/liborbit.so.0 exe= ;;
esac

echo "== another build's liborbit" >&2
# shellcheck disable=SC2086 # $pgo is one word or none
ORBIT_BUILD_SALT=check-shared-layout ./build.sh $pgo "$@" --out "$other"
echo "== the shared layout" >&2
# shellcheck disable=SC2086
./build.sh $pgo "$@" --out "$out"

echo "== library swap" >&2
cmp -s "$out/$library" "$other/$library" && {
    echo "check-shared-layout.sh: the two builds' liborbit are the same file" >&2
    exit 1
}
cp "$out/$library" "$other/$library"
for program in orbit orbitc orbit-lsp; do
    case "$program" in
        orbit-lsp) flag=--version ;;
        *) flag=-v ;;
    esac
    "$out/bin/$program$exe" "$flag" > /dev/null || {
        echo "check-shared-layout.sh: $program does not run with its own liborbit" >&2
        exit 1
    }
    if message=$("$other/bin/$program$exe" "$flag" 2>&1); then
        echo "check-shared-layout.sh: $program ran with another build's liborbit" >&2
        exit 1
    fi
    case "$message" in
        *"liborbit is from another build"*) ;;
        *)
            echo "check-shared-layout.sh: $program failed with another build's liborbit," >&2
            echo "but not by naming it:" >&2
            echo "$message" >&2
            exit 1
            ;;
    esac
done
rm -rf "$other"
echo "the programs refuse another build's liborbit" >&2

echo "== program tests on the layout" >&2
# shellcheck disable=SC2086 # $test_features holds plain words
ORBIT_BIN_DIR="$out/bin" cargo test --locked -p orbit-cli -p orbit-lsp $test_features

case "$library" in
    lib/liborbit.so.0)
        echo "== C host tests on the layout" >&2
        ORBIT_LIBORBIT="$out/$library" cargo test --locked -p orbit-capi \
            --features "orbit-capi/debugger${test_features:+,orbit/lua-5-4}" --test chost -- --ignored
        ;;
esac
