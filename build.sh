#!/usr/bin/env sh
# Builds OrbitScript for one target, optimized with a PGO profile when one
# is available (tests/README.md, "Profile-guided builds").
#
#   ./build.sh [options]
#
#   --target TRIPLE   the target to build for (default: this machine's)
#   --lua VERSION     the Lua version the build implements: 5.5 (default) or
#                     5.4 (the `lua-5-4` feature, ADR-14), for the language,
#                     the libraries and the C API alike. A 5.4 build has its
#                     own target directory (target/package-lua5.4), default
#                     --out (target/package-lua5.4/out) and default profile
#                     (pgo/<target>-lua5.4.profdata), and installs Lua 5.4's
#                     headers and library names
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
# The shared layout (ADR-10): liborbit, a Rust `dylib`, holds the runtime
# once, with Rust's standard library inside it. `orbit`, `orbitc` and the
# language server `orbit-lsp` are built with their `shared` feature, link
# it as a Rust crate and find it through their run path, and Lua's library
# names are aliases of it, so programs built for that Lua use it too.
# Stable Rust has no link-time optimization for a `dylib`, so this layout
# is built without it and relies on the profile for its speed: built
# without a profile it can be up to 30% slower than the static layout
# (the script says so when that happens). The programs and liborbit must
# come from one build: they compare their build's identity when they
# start (tools/check-shared-layout.sh tests that, and the layout). The
# names below are a 5.5 build's; a 5.4 build has the same with 5.4
# (lua54.dll). The formatter `orbit-fmt` doesn't link the runtime and is the
# same in both.
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
out=
lua=5.5
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
        --lua) lua=${2:?--lua needs a value}; shift 2 ;;
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

case "$lua" in
    5.5) package=package profile_suffix= ;;
    5.4) package=package-lua5.4 profile_suffix=-lua5.4 ;;
    *) echo "build.sh: --lua must be 5.5 or 5.4, not $lua" >&2; exit 2 ;;
esac
[ -n "$out" ] || out="$root/target/$package/out"
# Lua's library names: 55 or 54, as in lua55.dll.
lua_nodot=$(printf '%s' "$lua" | tr -d .)

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
    pgo="$root/pgo/$triple$profile_suffix.profdata"
fi
cd "$root"

target="$root/target/$package"

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
lib=$(defaults orbit-lib),debugger
lsp=$(defaults orbit-lsp)
[ "$ffi" = yes ] || {
    cli_static=$(without "$cli_static" ffi ffi-system-libffi)
    lib=$(without "$lib" ffi system-libffi)
}
[ "$debugger" = yes ] || {
    cli_static=$(without "$cli_static" dap debugger)
    lib=$(without "$lib" debugger)
}
[ "$system_libffi" = no ] || {
    cli_static=$(join "$cli_static" ffi-system-libffi)
    lib=$(join "$lib" system-libffi)
}
cli_extra= lib_extra= lsp_extra= fmt_extra=
# The Lua version selects the whole build's (ADR-14): every package that
# links the runtime gets the feature.
[ "$lua" = 5.5 ] || features="$features lua-5-4,orbit-lib/lua-5-4,orbit-lsp/lua-5-4"
for f in $(printf '%s' "$features" | tr ',' ' '); do
    case "$f" in
        orbit-lib/*) lib_extra=$(join "$lib_extra" "$f") ;;
        orbit-lsp/*) lsp_extra=$(join "$lsp_extra" "$f") ;;
        orbit-fmt/*) fmt_extra=$(join "$fmt_extra" "$f") ;;
        *) cli_extra=$(join "$cli_extra" "$f") ;;
    esac
done
cli_static=$(join "$cli_static" "$cli_extra")
lib=$(join "$lib" "$lib_extra")
lsp=$(join "$lsp" "$lsp_extra")
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
fmt_args="-p orbit-fmt $(features_arg "$fmt_extra")"
# The features $2 as Cargo names them when several packages are built at
# once: PKG/FEATURE for the package $1's own, and DEP/FEATURE as it is.
qualified() {
    list=
    for f in $(printf '%s' "$2" | tr ',' ' '); do
        case "$f" in
            */*) list=$(join "$list" "$f") ;;
            *) list=$(join "$list" "$1/$f") ;;
        esac
    done
    printf '%s' "$list"
}
# The shared layout: liborbit and the programs that link it, in ONE Cargo
# invocation. Cargo then settles on one feature set for every crate inside
# liborbit; built by separate commands, the second would rebuild liborbit
# with other features, and the programs would disagree about it. The
# programs have the static layout's features, plus `shared`.
shared_features=$(qualified orbit-lib "$lib")
shared_features=$(join "$shared_features" "$(qualified orbit-cli "$(join "$cli_static" shared)")")
shared_features=$(join "$shared_features" "$(qualified orbit-lsp "$(join "$lsp" shared)")")
shared_args="-p orbit-lib -p orbit-cli -p orbit-lsp --no-default-features --features $shared_features"

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

# The shared layout's builds. Two things differ from the static ones.
#
# No link-time optimization: stable rustc has none for a `dylib`. The
# profile's own setting (`lto = true` in Cargo.toml) stays for static builds.
#
# Rust's standard library goes into liborbit. Left alone, rustc sees the
# toolchain's shared `libstd` and has the programs use that one, which then
# clashes with the copy inside liborbit (and we don't ship a `libstd`).
# Naming `std`'s static library leaves rustc only that one. lay_out checks
# the result.
profile_var=$(printf '%s' "$profile" | tr 'a-z-' 'A-Z_')
shared_env="CARGO_PROFILE_${profile_var}_LTO=off"
std_flags() {
    libdir="$(rustc --print sysroot)/lib/rustlib/$triple/lib"
    rlib=$(ls "$libdir"/libstd-*.rlib 2> /dev/null | head -n 1)
    [ -n "$rlib" ] || {
        echo "build.sh: no Rust standard library for $triple: rustup target add $triple" >&2
        exit 1
    }
    # Toolchains that keep the metadata in its own file need both named.
    rmeta=${rlib%.rlib}.rmeta
    # rustc reads the flags from the environment, where Git Bash doesn't
    # translate paths.
    if command -v cygpath > /dev/null 2>&1; then
        rlib=$(cygpath -w "$rlib")
        rmeta=$(cygpath -w "$rmeta")
    fi
    [ ! -f "$rmeta" ] || printf -- '--extern\nstd=%s\n' "$rmeta"
    printf -- '--extern\nstd=%s\n' "$rlib"
}
# `build` for the shared layout: the same arguments.
build_shared() {
    cargo_args=
    while [ $# -gt 0 ] && [ "$1" != -- ]; do
        cargo_args="$cargo_args $1"
        shift
    done
    [ $# -gt 0 ] && shift
    old_ifs=$IFS
    IFS='
'
    # shellcheck disable=SC2046 # one flag per line
    set -- $(std_flags) "$@"
    IFS=$old_ifs
    # shellcheck disable=SC2086 # plain words
    env "$shared_env" CARGO_TARGET_DIR="$target" CARGO_ENCODED_RUSTFLAGS="$(encoded_rustflags "$@")" \
        "$cargo" build --profile "$profile" --target "$triple" $cargo_args
}
use_profile_shared() {
    build_shared "$@" -- "-Cprofile-use=$pgo" "-Cllvm-args=-vectorizer-min-trip-count=1"
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
# Lua's headers are per version (ADR-14); orbit.h is shared.
capi_include="$root/crates/orbit-capi/csrc/include"
headers="$capi_include/$lua"
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
            for alias in liborbit.dylib "liblua.$lua.dylib" "liblua$lua.dylib"; do
                ln -s "$lib" "$dir/lib/$alias"
            done
            files="bin/orbit bin/orbitc lib/$lib"
            ;;
        *-windows-*)
            for f in orbit.exe orbitc.exe orbit_lib.dll; do
                cp "$built/$f" "$dir/bin/$f"
            done
            luadll=lua$lua_nodot.dll
            def="$target/lua$lua_nodot.def"
            {
                echo "LIBRARY $luadll"
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
                    "/DEF:$(cygpath -w "$def")" "/OUT:$(cygpath -w "$dir/bin/$luadll")"
            else
                "${CC:-gcc}" -shared -nostdlib -o "$dir/bin/$luadll" "$def"
            fi
            files="bin/orbit.exe bin/orbitc.exe bin/orbit_lib.dll bin/$luadll"
            ;;
        *)
            cp "$built/orbit" "$built/orbitc" "$dir/bin/"
            lib=liborbit.so.0
            cp "$built/liborbit_lib.so" "$dir/lib/$lib"
            for alias in liborbit.so "liblua$lua.so.0" "liblua-$lua.so" "liblua.so.$lua"; do
                ln -s "$lib" "$dir/lib/$alias"
            done
            files="bin/orbit bin/orbitc lib/$lib"
            ;;
    esac
    for h in lua.h luaconf.h lualib.h lauxlib.h; do
        cp "$headers/$h" "$dir/include/$h"
    done
    cp "$capi_include/orbit.h" "$dir/include/orbit.h"
    # Nothing may ask for the toolchain's shared `libstd` (std_flags): its
    # name would be among the libraries the file needs.
    for f in $files; do
        if grep -a -q -E '(lib)?std-[0-9a-f]{16}\.(so|dylib|dll)' "$dir/$f"; then
            echo "build.sh: $f depends on Rust's shared standard library, which is not" >&2
            echo "part of the layout: liborbit must hold it. This toolchain did not take" >&2
            echo "the static one named with --extern std (see std_flags)." >&2
            exit 1
        fi
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
    # its `orbit`. The two builds' crates have different features, so
    # different symbol names: with a profile of the static build alone,
    # liborbit finds nothing for most of its functions and runs 1.1-1.3x
    # slower. The first instrumented build clears the raw counts of earlier
    # runs.
    raw="$target/pgo-profiles"
    instrument() {
        # shellcheck disable=SC2086 # $1 holds plain words
        CARGO_TARGET_DIR="$target" "$cargo" pgo instrument --profiles-dir "$raw" $2 build -- \
            --profile "$profile" --target "$triple" $1
    }
    # The shared layout's build (build_shared). cargo-pgo adds its flags to
    # RUSTFLAGS, which separates flags by spaces.
    instrument_shared() {
        flags=$(std_flags | tr '\n' ' ')
        case "$(std_flags)" in
            *" "*)
                echo "build.sh: --pgo-train can't pass the toolchain's path to cargo-pgo:" >&2
                echo "it has a space in it ($(rustc --print sysroot))" >&2
                exit 1
                ;;
        esac
        # shellcheck disable=SC2086 # $1 holds plain words
        env "$shared_env" CARGO_TARGET_DIR="$target" RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags" \
            "$cargo" pgo instrument --profiles-dir "$raw" --keep-profiles build -- \
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
    instrument_shared "$shared_args"
    import_lib
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
    # Both layouts: their crates are built with different features, so the
    # profile holds each function once per layout. `orbit` only: training
    # never runs `orbitc` or `orbit-lsp`, so the profile has nothing for the
    # functions only they have.
    # shellcheck disable=SC2086 # plain words
    build $static_args --bin orbit -- "-Cprofile-use=$pgo" "-Cllvm-args=-vectorizer-min-trip-count=1" \
        -Cllvm-args=-pgo-warn-missing-function 2> "$log" || { cat "$log" >&2; exit 1; }
    # shellcheck disable=SC2086 # plain words
    build_shared $shared_args --lib --bin orbit -- "-Cprofile-use=$pgo" "-Cllvm-args=-vectorizer-min-trip-count=1" \
        -Cllvm-args=-pgo-warn-missing-function 2>> "$log" || { cat "$log" >&2; exit 1; }
    # Only our crates: LLVM warns about dependencies too.
    stale='^warning: orbit.*\((hash mismatch)\|no profile data available for function\)'
    grep "$stale" "$log" | sed 's/^warning: /  /' | sort -u || true
    changed=$(grep '^warning: orbit.*(hash mismatch)' "$log" | sort -u | wc -l | tr -d ' ')
    new=$(grep '^warning: orbit.*no profile data available for function' "$log" | sort -u | wc -l | tr -d ' ')
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
    run_shared=use_profile_shared
else
    [ "$no_pgo" = yes ] || echo "== no profile at $pgo: building without one"
    run=build
    run_shared=build_shared
    if [ "$layout" = shared ]; then
        echo "== note: the shared layout has no link-time optimization (liborbit is a" >&2
        echo "   Rust dylib), so without a profile it can be up to 30% slower than the" >&2
        echo "   static layout. Pass a profile with --pgo FILE (--pgo-train records one" >&2
        echo "   on this machine; a profile captured on the target device works too)," >&2
        echo "   or build the static layout with --static." >&2
    fi
fi
if [ "$layout" = static ]; then
    # shellcheck disable=SC2086 # plain words
    $run $static_args
    # shellcheck disable=SC2086 # plain words
    $run $lsp_static_args
else
    # shellcheck disable=SC2086 # plain words
    $run_shared $shared_args
    import_lib
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
