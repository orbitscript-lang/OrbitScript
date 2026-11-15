#!/usr/bin/env sh
# Checks that docs/features.md lists every Cargo feature of the workspace's
# crates, and no others, and that its "Default" column agrees with each
# crate's `default` list.
#
#   tools/check-features.sh
#
# Reads each crates/*/Cargo.toml's [features] table, one `name = [...]` per
# line as the manifests write them, and the rows of the page's table:
# | `crate` | `feature` | yes or no | ...
# Prints what differs and fails if anything does.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for manifest in crates/*/Cargo.toml; do
    crate=$(sed -n 's/^name = "\(.*\)"$/\1/p' "$manifest" | head -n 1)
    awk -v crate="$crate" '
        /^\[/ { in_features = ($0 == "[features]"); next }
        !in_features || /^[[:space:]]*(#|$)/ { next }
        {
            name = $1
            if (name == "default") {
                line = $0
                sub(/^[^[]*\[/, "", line); sub(/\].*$/, "", line)
                gsub(/[" ]/, "", line)
                n = split(line, list, ",")
                for (i = 1; i <= n; i++) if (list[i] != "") on[list[i]] = 1
            } else {
                names[name] = 1
            }
        }
        END { for (f in names) printf "%s %s %s\n", crate, f, (f in on) ? "yes" : "no" }
    ' "$manifest"
done | sort > "$tmp/cargo"

# Table rows whose first two cells are code spans: crate, feature, default.
sed -n 's/^| *`\([a-z0-9-]*\)` *| *`\([a-z0-9-]*\)` *| *\([a-z]*\) *|.*/\1 \2 \3/p' \
    docs/features.md | sort > "$tmp/docs"

if ! cmp -s "$tmp/cargo" "$tmp/docs"; then
    echo "docs/features.md doesn't match the crates' Cargo.toml files." >&2
    echo "Only in Cargo.toml (crate, feature, default):" >&2
    comm -23 "$tmp/cargo" "$tmp/docs" | sed 's/^/  /' >&2
    echo "Only in docs/features.md:" >&2
    comm -13 "$tmp/cargo" "$tmp/docs" | sed 's/^/  /' >&2
    exit 1
fi
echo "docs/features.md lists all $(wc -l < "$tmp/cargo" | tr -d ' ') features."
