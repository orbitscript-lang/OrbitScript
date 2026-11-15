#!/usr/bin/env sh
# Proves the AddressSanitizer run (tests/asan/run.sh): each planted bug
# must be reported.
#
#   tests/asan/check-planted.sh [--toolchain NAME] [NAME...]
#
# Each planted/NAME.patch plants one memory bug in code that may contain
# `unsafe` (its first lines say which) and names, on its `Parts:` line, the
# parts of run.sh whose tests reach it. For each patch the script applies
# it, runs those parts (rebuilding the layout, so the bug is in the liborbit
# under test), expects a sanitizer report, and takes the patch back out. A
# bug the sanitizer doesn't report means those tests don't reach the code,
# or the sanitizer no longer sees it, and the script fails. So does a patch
# that no longer applies: rewrite it for the code as it is.
#
# It needs a clean working tree under crates/, which it changes and
# restores.
set -eu
cd "$(dirname "$0")/../.."
dir=tests/asan/planted

toolchain=nightly
if [ "${1:-}" = "--toolchain" ]; then
    toolchain=${2:?--toolchain needs a value}
    shift 2
fi

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
    parts=$(sed -n 's/^Parts: //p' "$p")
    if ! git apply "$p" 2>"$log"; then
        echo "$name: the patch no longer applies"
        sed 's/^/    /' "$log"
        bad=1
        continue
    fi
    applied=$p
    # shellcheck disable=SC2086 # $parts is a list of words
    if tests/asan/run.sh --toolchain "$toolchain" $parts >"$log" 2>&1; then
        echo "$name: not reported (parts: $parts)"
        bad=1
    elif grep -q "ERROR: AddressSanitizer" "$log"; then
        first=$(grep -m1 "ERROR: AddressSanitizer" "$log" | sed 's/.*ERROR: AddressSanitizer: //')
        echo "$name: reported: $first"
    elif grep -q "error\[E\|could not compile" "$log"; then
        echo "$name: the patched code doesn't compile"
        grep -A5 "^error" "$log" | sed 's/^/    /' | head -20
        bad=1
    else
        echo "$name: tests failed, but the sanitizer reported nothing (parts: $parts)"
        tail -20 "$log" | sed 's/^/    /'
        bad=1
    fi
    git apply -R "$p"
    applied=""
done
rm -f "$log"

if [ "$bad" -ne 0 ]; then
    exit 1
fi
echo "Every planted bug was reported."
