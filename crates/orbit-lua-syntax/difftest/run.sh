#!/bin/sh
# Differential tests of orbit-lua-syntax against a reference lua5.5; see README.md.
# Usage: run.sh LUA SUITE_DIR [SEEDS]
set -eu
LUA=$1
SUITE=$2
SEEDS=${3:-"1 2 3"}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
WORK=$ROOT/target/difftest
mkdir -p "$WORK/printed"
cargo build --quiet --release -p orbit-lua-syntax --example difftest
BIN=$ROOT/target/release/examples/difftest
status=0

echo "== round trip: printed ASTs compile to the same bytecode"
"$BIN" print "$WORK/printed" "$SUITE"/*.lua
names=$(cd "$SUITE" && ls *.lua | sed 's/\.lua$//')
# shellcheck disable=SC2086
"$LUA" "$HERE/difftest.lua" compare "$SUITE" "$WORK/printed" $names || status=1

compare() { # name generator-args...
  name=$1; shift
  python3 "$@"
  "$BIN" messages "$WORK/corpus" > "$WORK/ours.txt"
  "$LUA" "$HERE/difftest.lua" messages "$WORK/corpus" > "$WORK/ref.txt"
  if cmp -s "$WORK/ref.txt" "$WORK/ours.txt"; then
    echo "$name: $(wc -l < "$WORK/ref.txt") chunks agree"
  else
    echo "$name: DIFFERENCES"; diff "$WORK/ref.txt" "$WORK/ours.txt" | head -20; status=1
  fi
}

for seed in $SEEDS; do
  echo "== seed $seed"
  compare mutations "$HERE/gen_mutations.py" "$WORK/corpus" "$seed" 20000 "$SUITE"
  compare scopes "$HERE/gen_scopes.py" "$WORK/corpus" "$seed" 20000
  compare random "$HERE/gen_random.py" "$WORK/corpus" "$seed" 50000
  python3 "$HERE/gen_consts.py" "$WORK/corpus" "$seed" 20000
  "$BIN" ctc "$WORK/corpus" vzq > "$WORK/ours.txt"
  "$LUA" "$HERE/difftest.lua" ctc "$WORK/corpus" vzq > "$WORK/ref.txt"
  if cmp -s "$WORK/ref.txt" "$WORK/ours.txt"; then
    echo "constants: $(wc -l < "$WORK/ref.txt") chunks agree"
  else
    echo "constants: DIFFERENCES"; status=1
  fi
done
exit $status
