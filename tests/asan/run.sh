#!/usr/bin/env bash
# Runs the tests of the code that may contain `unsafe` under
# AddressSanitizer, built with a nightly toolchain: a test tool only, run by
# the weekly sanitizer workflow and never by pull requests (CLAUDE.md, the
# stable-only exception). It sees what valgrind (tests/memcheck) can't,
# reads and writes past variables on the stack, Rust's past a C caller's.
#
#   tests/asan/run.sh [--toolchain NAME] [--layout DIR] [PART...]
#
# Parts, all by default:
#   unit   the tests of orbit-std (with ffi), orbit-ffi, orbit-locale,
#          orbit-virtmem, orbit-capi and orbit-dap, built with the sanitizer.
#          orbit-gc is left out: its only `unsafe` hands memory back to the
#          allocator, which the sanitizer replaces.
#   chost  the C host tests (crates/orbit-capi/tests/chost), the host built
#          with -fsanitize=address, on a liborbit built with the sanitizer.
#
# The layout for `chost` is built into target/asan/layout unless --layout
# names one built the same way. The toolchain is `nightly` unless named.
# Any report fails the run.
#
# Both link the system's libffi (libffi-dev) instead of building the bundled
# copy, which the cc crate would build with the sanitizer too: libffi's
# assembly runs the called function on the stack block its C part
# allocates, red zones included, and the sanitizer then reports the stale
# red zones in whatever code next uses that stack. The system's libffi is
# not instrumented, and every access our code makes still is.
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD

toolchain=nightly
layout=""
parts=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        --toolchain) toolchain=${2:?--toolchain needs a value}; shift 2 ;;
        --layout) layout=${2:?--layout needs a value}; shift 2 ;;
        unit | chost) parts+=("$1"); shift ;;
        *) echo "tests/asan/run.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
[ "${#parts[@]}" -gt 0 ] || parts=(unit chost)

triple=$(rustc +"$toolchain" -vV | sed -n 's/^host: //p')
# Only the target is instrumented with an explicit --target: build scripts
# and proc macros run in the compiler, which has no sanitizer runtime.
export RUSTFLAGS="-Zsanitizer=address"
# A test that ends its program without closing a state leaves memory
# behind on purpose; leaks are not what this run looks for.
export ASAN_OPTIONS="detect_leaks=0"

for part in "${parts[@]}"; do
    case "$part" in
        unit)
            echo "== unit tests under AddressSanitizer" >&2
            # Not the doctests: rustdoc builds them without the sanitizer.
            cargo +"$toolchain" test --locked --lib --tests --target "$triple" --target-dir target/asan \
                -p orbit-std --features orbit-std/ffi,orbit-ffi/system-libffi -p orbit-ffi -p orbit-locale \
                -p orbit-virtmem -p orbit-capi -p orbit-dap
            ;;
        chost)
            if [ -z "$layout" ]; then
                echo "== a liborbit built with AddressSanitizer" >&2
                RUSTUP_TOOLCHAIN=$toolchain ./build.sh --no-pgo --system-libffi --target "$triple" \
                    --out "$root/target/asan/layout" >/dev/null
                layout=$root/target/asan/layout
            fi
            echo "== the C host tests under AddressSanitizer" >&2
            # The test harness itself needs no sanitizer: the host program
            # and the library it loads have it.
            env -u RUSTFLAGS ORBIT_CHOST_SANITIZE=address \
                ORBIT_LIBORBIT="$(cd "$layout" && pwd)/lib/liborbit.so.0" \
                cargo test --locked -p orbit-capi --features orbit-capi/debugger \
                --test chost -- --ignored
            ;;
    esac
done
echo "asan: no reports" >&2
