#!/usr/bin/env sh
# Builds OrbitScript for one target, optimized with a PGO profile when one
# is available (tests/README.md, "Profile-guided builds").
#
#   ./build.sh [options]
#
#   --target TRIPLE   the target to build for (default: this machine's)
#   --static          lay out the static `orbit`, `orbitc`, `orbit-lsp` and
#                     `orbit-fmt` instead of the shared layout
#   --out DIR         where to lay it out (default target/package/out)
#   --profile NAME    the cargo profile (default release)
#   --pgo FILE        optimize with the profile FILE (default
#                     pgo/<target>.profdata, when it exists); a profile
#                     captured on a target device works the same way
#   --no-pgo          build without a profile
#   --pgo-train       first record a new profile with cargo-pgo and write it
#                     to pgo/<target>.profdata (or the --pgo FILE); only for
#                     this machine's target, since it runs what it builds
#   --pgo-check       instead of laying anything out, list the functions of
#                     our crates the profile no longer covers; fails if any
#
# What goes into the programs (the defaults are what releases ship):
#   --system-libffi   link the system's shared libffi instead of building the
#                     bundled copy and linking it statically (pkg-config must
#                     find libffi); Apple targets always use the system's and
#                     MSVC targets always build the bundled copy
#   --no-ffi          leave out the `std.ffi` module and libffi altogether
#   --no-debugger     leave out the debugger: `orbit --debug-server` and the
#                     other debug options, and liborbit's DAP server
#   --features LIST   more Cargo features, separated by commas: PKG/FEATURE
#                     goes to that package's build when PKG is orbit-cli,
#                     orbit-lib, orbit-lsp or orbit-fmt; a bare FEATURE, or
#                     DEP/FEATURE for one of orbit-cli's dependencies, goes to
#                     the `orbit` and `orbitc` build (for example
#                     --features orbit-lib/system-libffi,orbit-cli/repl).
#                     May be given more than once
# The options apply to PGO training and checking too, so a profile trained
# with them matches the build that uses it. The committed profiles are
# trained with the defaults; with other options --pgo-check reports the
# functions that differ.
#
# The shared layout (ADR-10): liborbit holds the runtime once, `orbit`,
# `orbitc` and the language server `orbit-lsp` are built with `orbit`'s
# `shared` feature and find it through their run path, and Lua 5.5's library
# names are aliases of it, so programs built for Lua 5.5 use it too. The
# formatter `orbit-fmt` doesn't link the runtime and is the same in both.
#   Linux:   bin/orbit, bin/orbitc, bin/orbit-lsp, bin/orbit-fmt
#            lib/liborbit.so.0, and liborbit.so, liblua5.5.so.0 (Debian),
#                liblua-5.5.so (Fedora), liblua.so.5.5 (upstream) linking to it
#   macOS:   bin/orbit, bin/orbitc, bin/orbit-lsp, bin/orbit-fmt
#            lib/liborbit.0.dylib, and liborbit.dylib, liblua.5.5.dylib,
#                liblua5.5.dylib linking to it
#   Windows: bin/orbit.exe, orbitc.exe, orbit-lsp.exe, orbit-fmt.exe,
#                orbit_lib.dll, and
#                lua55.dll, which forwards Lua's names to orbit_lib.dll (a
#                copy would be a second runtime in the process); run from
#                Git Bash or MSYS2
#   all:     include/lua.h, luaconf.h, lualib.h, lauxlib.h, orbit.h
# The static layout: bin/orbit, bin/orbitc, bin/orbit-lsp and bin/orbit-fmt,
# one file each.
#
# Builds always name the target (`cargo build --target`), as cargo-pgo's do:
# rustflags then reach only the target's code, not build scripts, and symbol
# names, which the profile is keyed on, match the training build's. They use
# their own target directory (target/package), so target/release is left
# alone. A plain `cargo build --release` is not profile-guided.
set -eu

root=$(cd "$(dirname "$0")" && pwd)
out="$root/target/package/out"
profile=release
triple=
layout=shared
pgo=
no_pgo=no
train=no
check=no
ffi=yes
system_libffi=no
debugger=yes
features=
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out=${2:?--out needs a value}; shift 2 ;;
        --profile) profile=${2:?--profile needs a value}; shift 2 ;;
        --target) triple=${2:?--target needs a value}; shift 2 ;;
        --static) layout=static; shift ;;
        --pgo) pgo=${2:?--pgo needs a value}; shift 2 ;;
        --no-pgo) no_pgo=yes; shift ;;
        --pgo-train) train=yes; shift ;;
        --pgo-check) check=yes; shift ;;
        --system-libffi) system_libffi=yes; shift ;;
        --no-ffi) ffi=no; shift ;;
        --no-debugger) debugger=no; shift ;;
        --features)
            features="$features ${2:?--features needs a value}"
            shift 2
            ;;
        -h|--help) sed -n '2,/^set -eu$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "build.sh: unknown option $1" >&2; exit 2 ;;
    esac
done

cargo=${CARGO:-cargo}
host=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$triple" ] || triple=$host
if [ "$no_pgo" = yes ] && { [ -n "$pgo" ] || [ "$train" = yes ] || [ "$check" = yes ]; }; then
    echo "build.sh: --no-pgo can't be combined with --pgo, --pgo-train or --pgo-check" >&2
    exit 2
fi
if [ "$ffi" = no ] && [ "$system_libffi" = yes ]; then
    echo "build.sh: --system-libffi and --no-ffi can't be combined" >&2
    exit 2
fi
if [ "$train" = yes ] && [ "$triple" != "$host" ]; then
    echo "build.sh: --pgo-train runs the programs it builds, so it only trains for" >&2
    echo "this machine's target ($host), not $triple. Train on a $triple machine," >&2
    echo "or capture a profile on the device, and pass it with --pgo." >&2
    exit 2
fi
if [ -n "$pgo" ]; then
    # Absolute already: /x, or C:\x and C:/x on Windows.
    case "$pgo" in /* | [A-Za-z]:[\\/]*) ;; *) pgo="$(pwd)/$pgo" ;; esac
elif [ "$no_pgo" = no ]; then
    pgo="$root/pgo/$triple.profdata"
fi
cd "$root"

target="$root/target/package"

# The environment's rustflags with "$@" appended, encoded for
# CARGO_ENCODED_RUSTFLAGS (flags separated by 0x1f), which cargo prefers
# over RUSTFLAGS when both are set.
sep=$(printf '\037')
encoded_rustflags() {
    flags=${CARGO_ENCODED_RUSTFLAGS:-}
    if [ -z "$flags" ] && [ -n "${RUSTFLAGS:-}" ]; then
        # shellcheck disable=SC2086 # split RUSTFLAGS on whitespace, as cargo does
        flags=$(printf "%s$sep" $RUSTFLAGS)
        flags=${flags%"$sep"}
    fi
    for f in "$@"; do
        flags="${flags:+$flags$sep}$f"
    done
    printf '%s' "$flags"
}

# `cargo build` for the target in $target, with the cargo arguments before
# `--` and the extra rustflags after it.
build() {
    args=
    while [ $# -gt 0 ] && [ "$1" != -- ]; do
        args="$args $1"
        shift
    done
    [ $# -gt 0 ] && shift
    # shellcheck disable=SC2086 # $args holds plain words
    CARGO_TARGET_DIR="$target" CARGO_ENCODED_RUSTFLAGS="$(encoded_rustflags "$@")" \
        "$cargo" build --profile "$profile" --target "$triple" $args
}

# The static `orbit` and `orbitc`, and the shared build: liborbit first, as
# the programs link against it. The language server is built after them,
# with the same profile (only its runtime is profiled), and the formatter,
# which has no runtime, with the same flags.
#
# Each build's features: what the options leave out is taken away from the
# package's defaults (read from its Cargo.toml, so a new default feature
# needs no change here), and what they add, or --features names, is added.
# The default features of the package in crates/$1, separated by commas.
defaults() {
    list=$(sed -n 's/^default = \[\(.*\)\]$/\1/p' "crates/$1/Cargo.toml" | tr -d '" ')
    [ -n "$list" ] || { echo "build.sh: no default features in crates/$1/Cargo.toml" >&2; exit 1; }
    printf '%s' "$list"
}
# $1 with the comma-separated features after it removed.
without() {
    kept=
    for f in $(printf '%s' "$1" | tr ',' ' '); do
        drop=no
        for g in $(shift; printf '%s ' "$@"); do
            [ "$f" != "$g" ] || drop=yes
        done
        [ "$drop" = yes ] || kept=${kept:+$kept,}$f
    done
    printf '%s' "$kept"
}
# $1 and $2 joined by a comma, either of them possibly empty.
join() {
    if [ -n "$1" ] && [ -n "$2" ]; then printf '%s,%s' "$1" "$2"; else printf '%s' "$1$2"; fi
}
cli_static=$(defaults orbit-cli)
cli_shared=shared,line-editing,dap
lib=$(defaults orbit-lib),debugger
[ "$ffi" = yes ] || {
    cli_static=$(without "$cli_static" ffi ffi-system-libffi)
    lib=$(without "$lib" ffi system-libffi)
}
[ "$debugger" = yes ] || {
    cli_static=$(without "$cli_static" dap debugger)
    cli_shared=$(without "$cli_shared" dap debugger)
    lib=$(without "$lib" debugger)
}
[ "$system_libffi" = no ] || {
    cli_static=$(join "$cli_static" ffi-system-libffi)
    lib=$(join "$lib" system-libffi)
}
cli_extra= lib_extra= lsp_extra= fmt_extra=
for f in $(printf '%s' "$features" | tr ',' ' '); do
    case "$f" in
        orbit-lib/*) lib_extra=$(join "$lib_extra" "$f") ;;
        orbit-lsp/*) lsp_extra=$(join "$lsp_extra" "$f") ;;
        orbit-fmt/*) fmt_extra=$(join "$fmt_extra" "$f") ;;
        *) cli_extra=$(join "$cli_extra" "$f") ;;
    esac
done
cli_static=$(join "$cli_static" "$cli_extra")
cli_shared=$(join "$cli_shared" "$cli_extra")
lib=$(join "$lib" "$lib_extra")
# `--features ""` is fine with cargo, but leaves a stray word in the log.
features_arg() {
    [ -z "$1" ] || printf -- '--features %s' "$1"
}
# The cargo arguments that build the package in crates/$1 with the features
# $2. While those include all its defaults, they are asked for as the
# defaults: the enabled features, `default` among them, are part of the
# hash in every symbol name, which the profile is keyed on.
package_args() {
    all=$(defaults "$1")
    # shellcheck disable=SC2046 # split the lists on their commas
    if [ -z "$(without "$all" $(printf '%s' "$2" | tr ',' ' '))" ]; then
        # shellcheck disable=SC2046
        printf -- '-p %s %s' "$1" "$(features_arg "$(without "$2" $(printf '%s' "$all" | tr ',' ' '))")"
    else
        printf -- '-p %s --no-default-features %s' "$1" "$(features_arg "$2")"
    fi
}
static_args=$(package_args orbit-cli "$cli_static")
lsp_static_args="-p orbit-lsp $(features_arg "$lsp_extra")"
lsp_shared_args="-p orbit-lsp --no-default-features $(features_arg "$(join shared "$lsp_extra")")"
fmt_args="-p orbit-fmt $(features_arg "$fmt_extra")"
lib_args=$(package_args orbit-lib "$lib")
shared_args="-p orbit-cli --no-default-features $(features_arg "$cli_shared")"

# The rustflags of a build optimized with the profile at $pgo.
#
# With a profile, LLVM's loop vectorizer treats a loop whose measured trip
# count is under 16 as tiny and vectorizes it with masked lanes instead of a
# scalar remainder. Without masked stores (x86-64 before AVX-512) each lane
# becomes a compare and a branch: clearing a few registers after a native
# returns grew to about 280 instructions and made coroutines 12% slower than
# a build without a profile. A minimum of 1 keeps the usual vector loop with
# a scalar remainder.
use_profile() {
    build "$@" -- "-Cprofile-use=$pgo" "-Cllvm-args=-vectorizer-min-trip-count=1"
}

built_dir() {
    case "$profile" in
        dev) echo "$target/$triple/debug" ;;
        *) echo "$target/$triple/$profile" ;;
    esac
}

# Windows: `orbit`'s shared backend links with `orbit_lib.lib`, while Cargo
# names a cdylib's import library `orbit_lib.dll.lib`. Copies it, next to
# it, under the name the linker looks for.
import_lib() {
    case "$triple" in
        *-windows-msvc)
            lib_dir=$(built_dir)
            cp "$lib_dir/orbit_lib.dll.lib" "$lib_dir/orbit_lib.lib"
            ;;
    esac
}

# The function or variable each LUA_API, LUALIB_API and LUAMOD_API
# declaration in our headers names, one per line.
headers="$root/crates/orbit-capi/csrc/include"
api_names() {
    grep -hE '^(LUA_API|LUALIB_API|LUAMOD_API) ' "$headers/lua.h" "$headers/lauxlib.h" "$headers/lualib.h" |
        awk '{ if (match($0, /(lua|luaL|luaopen)_[a-z][A-Za-z0-9_]*/)) print substr($0, RSTART, RLENGTH) }' |
        sort -u
}

# MSVC's `link`: the one on the path, unless that is another program of the
# same name (Git Bash's own `link`), else the newest Visual Studio's, which
# vswhere finds. Fails when there is none.
msvc_link() {
    if command -v link.exe > /dev/null 2>&1 && link.exe /? 2>&1 | grep -q Microsoft; then
        echo link.exe
        return
    fi
    vswhere="$(cygpath -u 'C:\Program Files (x86)')/Microsoft Visual Studio/Installer/vswhere.exe"
    [ -x "$vswhere" ] || return 1
    vs=$("$vswhere" -latest -products '*' -property installationPath | tr -d '\r')
    [ -n "$vs" ] || return 1
    vs=$(cygpath -u "$vs")
    version=$(tr -d '\r' < "$vs/VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt") || return 1
    case "$host" in
        aarch64-*) arch=arm64 ;;
        *) arch=x64 ;;
    esac
    link="$vs/VC/Tools/MSVC/$version/bin/Host$arch/$arch/link.exe"
    [ -x "$link" ] || return 1
    echo "$link"
}

# Lays out the last build of layout $1 (static or shared) in $2, and sets
# $files to the files it copied, relative to $2.
lay_out() {
    built=$(built_dir)
    dir=$2
    rm -rf "$dir"
    mkdir -p "$dir/bin"
    case "$triple" in
        *-windows-*) exe=.exe ;;
        *) exe= ;;
    esac
    if [ "$1" = static ]; then
        cp "$built/orbit$exe" "$built/orbitc$exe" "$dir/bin/"
        files="bin/orbit$exe bin/orbitc$exe"
        return
    fi
    mkdir -p "$dir/lib" "$dir/include"
    case "$triple" in
        *-apple-*)
            cp "$built/orbit" "$built/orbitc" "$dir/bin/"
            lib=liborbit.0.dylib
            cp "$built/liborbit_lib.dylib" "$dir/lib/$lib"
            for alias in liborbit.dylib liblua.5.5.dylib liblua5.5.dylib; do
                ln -s "$lib" "$dir/lib/$alias"
            done
            files="bin/orbit bin/orbitc lib/$lib"
            ;;
        *-windows-*)
            for f in orbit.exe orbitc.exe orbit_lib.dll; do
                cp "$built/$f" "$dir/bin/$f"
            done
            def="$target/lua55.def"
            {
                echo "LIBRARY lua55.dll"
                echo "EXPORTS"
                api_names | sed 's/.*/  & = orbit_lib.&/'
            } > "$def"
            # A DLL with no code, only forwarded exports, linked with the
            # toolchain's linker: MSVC's `link` where there is one, else
            # MinGW's gcc. With no object files `link` can't tell the
            # machine, so it is named.
            case "$triple" in
                aarch64-*) machine=ARM64 ;;
                *) machine=X64 ;;
            esac
            if link=$(msvc_link); then
                # Git Bash would turn the /OPTIONS into paths.
                MSYS2_ARG_CONV_EXCL='*' "$link" /NOLOGO /DLL /NOENTRY "/MACHINE:$machine" \
                    "/DEF:$(cygpath -w "$def")" "/OUT:$(cygpath -w "$dir/bin/lua55.dll")"
            else
                "${CC:-gcc}" -shared -nostdlib -o "$dir/bin/lua55.dll" "$def"
            fi
            files="bin/orbit.exe bin/orbitc.exe bin/orbit_lib.dll bin/lua55.dll"
            ;;
        *)
            cp "$built/orbit" "$built/orbitc" "$dir/bin/"
            lib=liborbit.so.0
            cp "$built/liborbit_lib.so" "$dir/lib/$lib"
            for alias in liborbit.so liblua5.5.so.0 liblua-5.5.so liblua.so.5.5; do
                ln -s "$lib" "$dir/lib/$alias"
            done
            files="bin/orbit bin/orbitc lib/$lib"
            ;;
    esac
    for h in lua.h luaconf.h lualib.h lauxlib.h orbit.h; do
        cp "$headers/$h" "$dir/include/$h"
    done
}

# Adds the language server and the formatter to the layout lay_out last
# wrote (PGO training lays out only the programs it runs).
lay_out_tools() {
    for tool in orbit-lsp orbit-fmt; do
        cp "$built/$tool$exe" "$dir/bin/"
        files="$files bin/$tool$exe"
    done
}

if [ "$train" = yes ]; then
    command -v cargo-pgo > /dev/null 2>&1 || {
        echo "build.sh: --pgo-train needs cargo-pgo: cargo install cargo-pgo" >&2
        exit 1
    }
    profdata="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata"
    [ -x "$profdata" ] || {
        echo "build.sh: --pgo-train needs llvm-profdata: rustup component add llvm-tools" >&2
        exit 1
    }
    # One profile covers both layouts: the static `orbit`, then liborbit and
    # its `orbit`. Trained on the static build alone, liborbit ran 1.3-2.1x
    # slower than the static build. The first instrumented build clears the
    # raw counts of earlier runs.
    raw="$target/pgo-profiles"
    instrument() {
        # shellcheck disable=SC2086 # $1 holds plain words
        CARGO_TARGET_DIR="$target" "$cargo" pgo instrument --profiles-dir "$raw" $2 build -- \
            --profile "$profile" --target "$triple" $1
    }
    workload() {
        LLVM_PROFILE_FILE="$raw/orbit_%m_%p.profraw" sh tools/pgo-workload.sh "$1"
    }
    echo "== training the static orbit"
    instrument "$static_args" ""
    lay_out static "$target/pgo-train/static"
    workload "$target/pgo-train/static/bin/orbit"
    echo "== training liborbit"
    instrument "$lib_args" --keep-profiles
    import_lib
    instrument "$shared_args" --keep-profiles
    lay_out shared "$target/pgo-train/shared"
    workload "$target/pgo-train/shared/bin/orbit"
    # `cargo pgo optimize` merges the counts with the same llvm-profdata, but
    # only as part of a build of its own.
    mkdir -p "$(dirname "$pgo")"
    "$profdata" merge -o "$pgo" "$raw"
    echo "== wrote $pgo"
fi

if [ "$check" = yes ]; then
    [ -f "$pgo" ] || { echo "build.sh: no profile at $pgo" >&2; exit 1; }
    # Its own target directory, as the extra flag rebuilds everything. Cargo
    # replays the warnings of the crates it doesn't rebuild.
    target="$root/target/pgo-check"
    mkdir -p "$target"
    log="$target/check.log"
    # `orbit` only: training never runs `orbitc`, so the profile has nothing
    # for the functions only it has.
    # shellcheck disable=SC2086 # plain words
    build $static_args --bin orbit -- "-Cprofile-use=$pgo" "-Cllvm-args=-vectorizer-min-trip-count=1" \
        -Cllvm-args=-pgo-warn-missing-function 2> "$log" || { cat "$log" >&2; exit 1; }
    # Only our crates: LLVM warns about dependencies too.
    stale='^warning: orbit.*\((hash mismatch)\|no profile data available for function\)'
    grep "$stale" "$log" | sed 's/^warning: /  /' || true
    changed=$(grep -c '^warning: orbit.*(hash mismatch)' "$log" || true)
    new=$(grep -c '^warning: orbit.*no profile data available for function' "$log" || true)
    echo "$changed changed, $new new or renamed"
    if [ "$changed" -eq 0 ] && [ "$new" -eq 0 ]; then
        echo "the profile matches the code"
        exit 0
    fi
    echo "the profile is stale: retrain with ./build.sh --pgo-train"
    exit 1
fi

if [ -n "$pgo" ] && [ -f "$pgo" ]; then
    echo "== optimizing with $pgo"
    run=use_profile
else
    [ "$no_pgo" = yes ] || echo "== no profile at $pgo: building without one"
    run=build
fi
if [ "$layout" = static ]; then
    # shellcheck disable=SC2086 # plain words
    $run $static_args
    # shellcheck disable=SC2086 # plain words
    $run $lsp_static_args
else
    # shellcheck disable=SC2086 # plain words
    $run $lib_args
    import_lib
    # shellcheck disable=SC2086 # plain words
    $run $shared_args
    # shellcheck disable=SC2086 # plain words
    $run $lsp_shared_args
fi
# shellcheck disable=SC2086 # plain words
$run $fmt_args
lay_out "$layout" "$out"
lay_out_tools
out=$(cd "$out" && pwd)

total=0
for f in $files; do
    n=$(wc -c < "$out/$f" | tr -d ' ')
    total=$((total + n))
    printf '%10s  %s\n' "$n" "$out/$f"
done
printf '%10s  total\n' "$total"
