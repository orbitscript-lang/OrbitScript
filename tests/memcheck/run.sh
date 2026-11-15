#!/usr/bin/env sh
# Runs the tests of the code that may contain `unsafe` under valgrind's
# memcheck (Linux), and fails on any report.
#
#   tests/memcheck/run.sh [--layout DIR] [--quick] [PART...]
#
# PART is one or more of chost, sanitize, cmod, unit, capi, liblua
# (default: all of them, in that order):
#
#   chost     the C host program's modes (crates/orbit-capi/tests/chost) on
#             the laid-out liborbit
#   sanitize  the same tests once more, the host built with
#             AddressSanitizer (ORBIT_CHOST_SANITIZE) and not under valgrind
#   cmod      the C module tests (orbit-cli's c_modules), on the layout's
#             orbit and, for the ignored one, on a second copy of the runtime
#   unit      the tests that run the FFI, locale, virtual memory and C API
#             code in the test process: orbit-std (ffi feature), orbit-ffi,
#             orbit-locale, orbit-virtmem, orbit-capi
#   capi      the C API programs (tests/capi, `cargo testkit capi`) on the
#             laid-out liborbit, which call every function it exports
#   liblua    the Lua suite through PUC's lua.c on liborbit in the suite's
#             soft mode, every file but all.lua (which runs them all again)
#             and heavy.lua (which grows a table until valgrind runs out of
#             memory first); also the only part that reaches the C library's
#             local time (orbit-std/src/ctime.rs, through os.date and os.time)
#
# The layout is built by ./build.sh without a profile into
# target/memcheck/layout, unless --layout names one already built. The
# liblua part needs the reference build (scripts/build-reference-lua.sh).
# --quick runs only the part of the suite each planted bug needs
# (tests/memcheck/check-planted.sh uses it).
#
# Each program runs under valgrind as its own process, never with
# --trace-children: what the tests start (the C compiler, `sh`) is not ours
# and is not checked, so no suppression list is needed. Every report goes to
# its own file under target/memcheck/logs; the script prints them and fails
# if any file is not empty, whatever the tests themselves said.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

layout=
quick=
parts=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --layout) layout=$2; shift 2 ;;
        --quick) quick=1; shift ;;
        chost | sanitize | cmod | unit | capi | liblua) parts="$parts $1"; shift ;;
        *) echo "run.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
[ -n "$parts" ] || parts="chost sanitize cmod unit capi liblua"

logs="$root/target/memcheck/logs"
rm -rf "$logs"
mkdir -p "$logs"
# -q prints errors only, so an empty log file means a clean process. Leaks
# are not checked: a program may leave memory to the OS at exit.
vg="valgrind -q --error-exitcode=99 --leak-check=no --log-file=$logs/%p.log"

if [ -z "$layout" ]; then
    layout="$root/target/memcheck/layout"
    ./build.sh --no-pgo --out "$layout" >&2
fi
layout=$(cd "$layout" && pwd)
lib="$layout/lib/liborbit.so.0"

failed=
step() {
    echo "== $1" >&2
}
# A step's own failure is noted, and the script goes on to the next, so one
# run reports everything.
note() {
    echo "memcheck: $1 failed" >&2
    failed="$failed $1"
}

for part in $parts; do
    case "$part" in
        chost)
            step "C host program under valgrind"
            ORBIT_LIBORBIT="$lib" ORBIT_TEST_WRAPPER="$vg" \
                cargo test --locked -p orbit-capi --features orbit-capi/debugger \
                --test chost -- --ignored || note chost
            ;;
        sanitize)
            step "C host program with AddressSanitizer"
            ORBIT_LIBORBIT="$lib" ORBIT_CHOST_SANITIZE=address \
                cargo test --locked -p orbit-capi --features orbit-capi/debugger \
                --test chost -- --ignored || note sanitize
            ;;
        cmod)
            step "C module tests under valgrind"
            ORBIT_BIN_DIR="$layout/bin" ORBIT_LIBORBIT="$lib" ORBIT_TEST_WRAPPER="$vg" \
                cargo test --locked -p orbit-cli --test c_modules -- --include-ignored || note cmod
            ;;
        unit)
            step "FFI, locale, virtual memory and C API tests under valgrind"
            # Cargo runs each test program through the runner; what the
            # tests start themselves runs as usual.
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="$vg" \
                cargo test --locked -p orbit-std --features orbit-std/ffi \
                -p orbit-ffi -p orbit-locale -p orbit-virtmem -p orbit-capi || note unit
            ;;
        capi)
            step "the C API programs under valgrind"
            cargo testkit capi --layout "$layout" --wrap "$vg" || note capi
            ;;
        liblua)
            step "the suite through liblua under valgrind (soft mode)"
            ref="$root/target/reference-lua"
            if [ -n "$quick" ]; then
                files="strings.lua api.lua"
            else
                files=$(cargo testkit conformance --list |
                    awk '{print $1}' | grep '\.lua$' | grep -v -x -e all.lua -e heavy.lua)
            fi
            # The suite's results are judged by the other jobs; this one
            # judges only valgrind's reports, so the run's own verdict is
            # ignored. The timeout allows for valgrind's slowdown.
            # shellcheck disable=SC2086 # $files is a list of words
            cargo testkit liblua --lib "$lib" --c-libs "$ref/libs" --soft \
                --wrap "$vg" --timeout 1800 $files || true
            ;;
    esac
done

step "valgrind's reports"
reports=0
for f in "$logs"/*.log; do
    [ -s "$f" ] || continue
    reports=$((reports + 1))
    echo "--- $(basename "$f")"
    cat "$f"
done
if [ "$reports" -gt 0 ]; then
    echo "memcheck: valgrind reported errors in $reports processes" >&2
    failed="$failed valgrind"
fi
if [ -n "$failed" ]; then
    echo "memcheck: failed:$failed" >&2
    exit 1
fi
echo "memcheck: no reports" >&2
