#!/bin/sh
# Sets the version a build reports, from a release tag:
#   tools/set-version.sh v0.1.0       (or v0.1.0-rc, v0.1.0-rc.2: prereleases)
# It writes the semantic version after the `v` into the workspace version in
# Cargo.toml, which every crate inherits (so `orbit -v` prints it), and into
# the VS Code extension's package.json, and brings both lockfiles up to date.
# The release workflow runs it before building; the files in the repository
# keep 0.0.0, so a release needs no version commit.
set -eu

tag=${1:?usage: tools/set-version.sh vX.Y.Z[-PRERELEASE]}
version=${tag#v}
# Semantic versioning 2.0: three numbers without leading zeros, then an
# optional prerelease of dot-separated alphanumerics and hyphens.
if ! printf '%s\n' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$' ||
    [ "$tag" = "$version" ]; then
    echo "set-version.sh: $tag is not v followed by a semantic version (v0.1.0, v0.1.0-rc)" >&2
    exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

# Only the version line of [workspace.package].
sed '/^\[workspace\.package\]/,/^\[/s/^version = ".*"/version = "'"$version"'"/' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml
grep -q "^version = \"$version\"" Cargo.toml || {
    echo "set-version.sh: found no version under [workspace.package] in Cargo.toml" >&2
    exit 1
}
# Rewrites only our own crates' entries in Cargo.lock.
${CARGO:-cargo} update --workspace --quiet

(cd editors/vscode && npm version --no-git-tag-version --allow-same-version "$version" > /dev/null)
echo "version $version"
