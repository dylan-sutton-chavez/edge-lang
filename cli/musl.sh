#!/bin/sh
set -eu
apk add -q --no-cache bash clang linux-headers llvm make zlib-static
export CC=clang
# build scripts and proc macros stay dynamic and only the interpreter binary links static in a target dir of its own
RUSTFLAGS="-C target-feature=-crt-static" cargo rustc --locked --release -p interpreter --features cli --bin interpreter --target-dir target/musl -- -C target-feature=+crt-static
