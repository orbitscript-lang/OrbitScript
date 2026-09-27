#!/usr/bin/env sh
# Replaces the vendored Lua test suite with the one at another tag.
#
#   scripts/update-lua-testes.sh v5.5.2
#
# Then review the diff of tests/lua-5.5/testes, recheck the line numbers in
# manifest.txt and SKIPS.md, and rerun the suite against the reference
# interpreter (scripts/build-reference-lua.sh reads the new VERSION).
set -eu
tag=${1:?usage: $0 TAG}
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" https://github.com/lua/lua "$tmp/lua"
dest="$root/tests/lua-5.5"
rm -rf "$dest/testes"
cp -R "$tmp/lua/testes" "$dest/testes"
(cd "$dest/testes" && find . -type f | LC_ALL=C sort | xargs sha256sum) > "$dest/SHA256SUMS"
echo "$tag $(git -C "$tmp/lua" rev-parse HEAD)" > "$dest/VERSION"
echo "Vendored testes/ from lua/lua $tag."
