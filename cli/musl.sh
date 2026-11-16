#!/bin/sh
set -eu
apk add -q --no-cache bash clang linux-headers llvm make zlib-static
export CC=clang
# build scripts and proc macros stay dynamic and only the interpreter binary links static
RUSTFLAGS="-C target-feature=-crt-static" cargo rustc --locked --release --bin interpreter -- -C target-feature=+crt-static
