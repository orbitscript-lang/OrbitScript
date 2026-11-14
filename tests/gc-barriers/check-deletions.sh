#!/usr/bin/env sh
# Proves the write-barrier tests in crates/orbit/tests/gc_barriers.rs.
#
#   tests/gc-barriers/check-deletions.sh [NAME...]
#
# Each NAME.patch beside this script deletes one write barrier (its first
# line says which) and names, on its second, the tests that depend on it.
# For each patch the script applies it, runs those tests in a debug build,
# where the heap verifier checks every marking, and expects every one of
# them to fail; then it takes the patch back out. A test that still passes
# with its barrier gone doesn't test that barrier, and the script fails.
# So does a patch that no longer applies: rewrite it for the code as it is.
#
# Run it on demand, and paste its output into the description of any pull
# request that changes barrier or collector code. It needs a clean working
# tree under crates/, which it changes and restores.
set -eu
cd "$(dirname "$0")/../.."
dir=tests/gc-barriers
run_tests() {
    # Every named test, exactly; non-zero if any fails.
    cargo test -q -p orbit --test gc_barriers -- --exact "$@"
}

if ! git diff --quiet -- crates; then
    echo "crates/ has uncommitted changes: commit or stash them first" >&2
    exit 2
fi

applied=""
restore() {
    if [ -n "$applied" ]; then
        git apply -R "$applied"
    fi
}
trap restore EXIT
trap 'exit 130' INT TERM

echo "Unmodified: the tests must pass."
if ! run_tests >/dev/null 2>&1; then
    run_tests || true
    echo "The barrier tests fail on unmodified code." >&2
    exit 1
fi

if [ "$#" -gt 0 ]; then
    patches=""
    for n in "$@"; do patches="$patches $dir/$n.patch"; done
else
    patches=$(ls "$dir"/*.patch)
fi

log=$(mktemp)
bad=0
for p in $patches; do
    name=$(basename "$p" .patch)
    tests=$(sed -n 's/^Tests: //p' "$p")
    if ! git apply "$p" 2>"$log"; then
        echo "$name: the patch no longer applies"
        sed 's/^/    /' "$log"
        bad=1
        continue
    fi
    applied=$p
    for t in $tests; do
        if run_tests "$t" >"$log" 2>&1; then
            echo "$name: $t still passes without the barrier"
            bad=1
        elif grep -q "orbit-gc verify" "$log"; then
            echo "$name: $t fails, reported by the heap verifier"
        elif grep -q "error\[E\|could not compile" "$log"; then
            echo "$name: the patched code doesn't compile"
            grep -A5 "^error" "$log" | sed 's/^/    /' | head -20
            bad=1
            break
        else
            echo "$name: $t fails (not through the verifier):"
            grep -m1 "panicked" -A1 "$log" | sed 's/^/    /'
        fi
    done
    git apply -R "$p"
    applied=""
done
rm -f "$log"
if [ "$bad" -ne 0 ]; then
    echo "Some barrier is not proven by its test." >&2
    exit 1
fi
echo "Every deleted barrier made its tests fail."
