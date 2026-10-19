#!/bin/sh
# Runs the PGO training workload with an instrumented `orbit`:
#   tools/pgo-workload.sh path/to/orbit
# The binary writes its own counts (to LLVM_PROFILE_FILE); this script only
# runs the programs. `./build.sh --pgo-train` runs it.
#
# Workload: bench/ at quick inputs, examples/ at their train= inputs
# (OrbitScript entries and their Lua twins), tests/diff, tests/diff/compat and
# tests/first-light. Programs that exit with an error still record counts.
#
# PGO_EXCLUDE leaves benchmarks or examples out, by file or directory name
# (PGO_EXCLUDE="fib.lua calc"), e.g. to time a program the profile has
# never seen.
set -u
orbit="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
root="$(cd "$(dirname "$0")/.." && pwd)"
# macOS has no `timeout`.
limit=
command -v timeout > /dev/null 2>&1 && limit="timeout 120"

run() {
    dir="$1"
    shift
    # shellcheck disable=SC2086 # $limit is a command and its argument
    if ! (cd "$dir" && $limit "$orbit" "$@" > /dev/null 2>&1); then
        echo "  $1 exited with an error (its counts are kept)"
    fi
}

excluded() {
    case " ${PGO_EXCLUDE:-} " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

# file  full=ARGS  quick=ARGS  # comment (ARGS separated by commas)
sed -e 's/#.*//' "$root/bench/manifest.txt" | while read -r file rest; do
    [ -n "$file" ] || continue
    excluded "$file" && continue
    quick=$(printf '%s\n' $rest | sed -n 's/^quick=//p' | tr ',' ' ')
    run "$root/bench" "$file" $quick
done

# dir  entry  train=ARGS  # comment (ARGS separated by commas)
sed -e 's/#.*//' "$root/examples/manifest.txt" | while read -r dir entry rest; do
    [ -n "$dir" ] || continue
    excluded "$dir" && continue
    args=$(printf '%s\n' $rest | sed -n 's/^train=//p' | tr ',' ' ')
    run "$root/examples/$dir" "$entry" $args
    twin="${entry%.orb}.lua"
    if [ "$twin" != "$entry" ] && [ -f "$root/examples/$dir/$twin" ]; then
        run "$root/examples/$dir" "$twin" $args
    fi
done

for corpus in tests/diff tests/diff/compat tests/first-light; do
    for f in "$root/$corpus"/*.lua; do
        run "$root/$corpus" "$(basename "$f")"
    done
done
exit 0
