#!/bin/sh
# link.sh OUT.so ARCHIVE : link the staticlib into a liblua-compatible .so
set -e
S=$(cd "$(dirname "$0")" && pwd)
LLD=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld
cc -shared -o "$1" -Wl,-soname,liblua5.5.so.0 -Wl,--whole-archive "$2" -Wl,--no-whole-archive \
  -Wl,--version-script=${MAP:-$S/liblua.map} -Wl,--gc-sections -Wl,-O1 -Wl,-z,relro,-z,now \
  -B$LLD -fuse-ld=lld -Wl,--icf=all -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
strip "$1"
