#!/bin/sh
# min of 5 pinned runs, ns per call
O=/home/claude/orbitscript/target/release/orbit; L=/home/claude/orbitscript/target/reference-lua/bin/lua5.5
N=5000000
row() { interp=$1; w=$2; best=; for k in 1 2 3 4 5; do v=$(taskset -c 2 $interp bench.lua $w $N 2>/dev/null | tail -1); case $v in ''|*[!0-9.]*) echo "n/a"; return;; esac; best=$(python3 -c "print(min(float('$v'), float('${best:-1e9}')))"); done; echo $best; }
for w in "$@"; do printf "%-18s orbit %6s   lua5.5 %6s\n" $w "$(row $O $w)" "$(row $L $w)"; done
