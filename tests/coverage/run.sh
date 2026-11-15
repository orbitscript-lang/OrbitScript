#!/usr/bin/env bash
# Measures which code the tests run (Linux), with the compiler's own
# instrumentation (`-C instrument-coverage`, stable), and checks the result
# with `cargo testkit coverage` (tests/coverage/README.md):
#
#   tests/coverage/run.sh [--update]
#
# 1. `cargo test --workspace` (orbit-std with ffi), built instrumented.
# 2. A layout ./build.sh builds instrumented: the suite, tests/diff (whose
#    locale.lua reaches most of the locale objects, given the locales the
#    reference-lua CI action generates) and the compatibility programs on
#    its `orbit`; the C host tests, the C API programs (tests/capi), the C
#    module tests and the suite through liblua on its liborbit.
#
# Every instrumented process writes a profile into target/cov/prof; they
# are merged and exported as target/cov/coverage.lcov, which the check
# reads. --update passes on to the check, to record the line coverage
# measured. Needs the llvm-tools component (`rustup component add
# llvm-tools`) and the reference build (scripts/build-reference-lua.sh).
set -euo pipefail
cd "$(dirname "$0")/../.."
root=$PWD

update=()
for a in "$@"; do
    case "$a" in
        --update) update=(--update) ;;
        *) echo "tests/coverage/run.sh: unknown argument $a" >&2; exit 2 ;;
    esac
done

tools="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
[ -x "$tools/llvm-cov" ] || {
    echo "tests/coverage/run.sh: no llvm-cov: rustup component add llvm-tools" >&2
    exit 2
}

cov=$root/target/cov
rm -rf "$cov/prof"
mkdir -p "$cov/prof"
export LLVM_PROFILE_FILE="$cov/prof/%p-%m.profraw"
export RUSTFLAGS="-C instrument-coverage"
export CARGO_TARGET_DIR="$cov/target"
ref=$root/target/reference-lua

echo "== the workspace's tests, instrumented" >&2
cargo test --locked --workspace --features orbit-std/ffi

echo "== an instrumented layout" >&2
layout=$cov/layout
./build.sh --no-pgo --out "$layout" >/dev/null
lib=$layout/lib/liborbit.so.0

# Only what these run is measured here; whether they pass is judged by the
# conformance job.
echo "== the suite, the differential tests and the compatibility programs" >&2
# The test kit itself is an ordinary build.
testkit() {
    env -u RUSTFLAGS -u CARGO_TARGET_DIR cargo run --release --locked -q -p orbit-testkit -- "$@"
}
testkit conformance --interp "$layout/bin/orbit" --c-libs "$ref/libs" || true
testkit diff --reference "$ref/bin/lua5.5" --interp "$layout/bin/orbit" || true
testkit diff --corpus tests/diff/compat --reference "$ref/bin/lua5.5" \
    --interp "$layout/bin/orbit" || true
echo "== the C host tests" >&2
ORBIT_LIBORBIT="$lib" cargo test --locked -p orbit-capi --features orbit-capi/debugger \
    --test chost -- --ignored
echo "== the C module tests" >&2
ORBIT_BIN_DIR="$layout/bin" ORBIT_LIBORBIT="$lib" \
    cargo test --locked -p orbit-cli --test c_modules -- --include-ignored
echo "== the C API programs" >&2
testkit capi --layout "$layout"
echo "== the suite through liblua" >&2
testkit liblua --lib "$lib" --c-libs "$ref/libs" || true

echo "== merging the profiles" >&2
"$tools/llvm-profdata" merge -sparse -o "$cov/coverage.profdata" "$cov"/prof/*.profraw
# One export per program or library: a function built differently in two of
# them (other features, another profile) has two records, and an export of
# all of them at once keeps only one, dropping the other's counts.
rm -rf "$cov/lcov"
mkdir -p "$cov/lcov"
n=0
while IFS= read -r f; do
    n=$((n + 1))
    "$tools/llvm-cov" export -format=lcov -skip-branches -instr-profile "$cov/coverage.profdata" \
        -object "$f" --ignore-filename-regex='/\.cargo/|/rustc/' \
        >"$cov/lcov/$n.lcov" 2>>"$cov/llvm-cov.log" || rm -f "$cov/lcov/$n.lcov"
done < <(find "$cov/target/debug" "$cov/target/debug/deps" -maxdepth 1 -type f -executable \
    ! -name '*.so' ! -name '*.d'; find "$layout/bin" "$layout/lib" -type f)

testkit coverage --lcov "$cov/lcov" "${update[@]}"
