#!/bin/sh
# Clones the programs in sources.txt at their pinned commits into
# target/bench-apps/NAME (outside version control). Safe to rerun: an
# existing checkout is moved to the pinned commit.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dest="$here/../../target/bench-apps"
mkdir -p "$dest"
grep -v '^#' "$here/sources.txt" | while read -r name commit url _licence; do
  [ -n "$name" ] || continue
  dir="$dest/$name"
  if [ ! -d "$dir/.git" ]; then
    git init -q "$dir"
    git -C "$dir" remote add origin "$url"
  fi
  if [ "$(git -C "$dir" rev-parse -q --verify HEAD 2>/dev/null)" != "$commit" ]; then
    echo "fetching $name"
    git -C "$dir" fetch -q --depth 1 origin "$commit"
    git -C "$dir" checkout -q --detach FETCH_HEAD
  fi
done
echo "programs are in target/bench-apps"
