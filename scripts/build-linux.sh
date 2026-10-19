#!/bin/sh
# Build multilink natively on Linux.
#
#   sh scripts/build-linux.sh
#
# Needs: Rust (rustup). Output: target/linux-$(uname -m)/{multilink, multilink.example.toml}
# Run with: sudo ./multilink run
set -eu
DIR=${CARGO_TARGET_DIR:-target}
OUT=$DIR/linux-$(uname -m)

cargo build --release

mkdir -p "$OUT"
cp "$DIR/release/multilink" multilink.example.toml "$OUT/"
echo "Bundle ready: $OUT (run with sudo)"
