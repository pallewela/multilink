#!/bin/sh
# Cross-compile a Windows build from macOS (or Linux) and bundle it with wintun.dll,
# which multilink.exe needs next to it at runtime.
#
#   sh scripts/build-windows.sh                                  # ARM64, e.g. UTM on Apple Silicon
#   TARGET=x86_64-pc-windows-msvc sh scripts/build-windows.sh    # x86-64 PCs
#
# Needs: rustup target add $TARGET && cargo install --locked cargo-xwin
# Output: target/windows-$TARGET/{multilink.exe, wintun.dll, wintun-LICENSE.txt, multilink.example.toml}
set -eu
TARGET=${TARGET:-aarch64-pc-windows-msvc}
case $TARGET in
    aarch64-*) ARCH=arm64 ;;
    x86_64-*) ARCH=amd64 ;;
    *) echo "unsupported TARGET $TARGET" >&2; exit 1 ;;
esac
DIR=${CARGO_TARGET_DIR:-target}
OUT=$DIR/windows-$TARGET

# ipstack's build script runs `cargo metadata`, which inherits the Windows
# rustflags and fails on the host rustc unless they are cleared.
shim=$(mktemp -d)
trap 'rm -rf "$shim"' EXIT
printf '#!/bin/sh\nunset CARGO_ENCODED_RUSTFLAGS\nexec %s "$@"\n' "$(command -v cargo)" > "$shim/cargo"
chmod +x "$shim/cargo"

PATH="$shim:$PATH" cargo xwin build --release --target "$TARGET"

# Official wintun release, pinned by version and SHA-256 (as published on wintun.net).
WINTUN_VERSION=0.14.1
WINTUN_SHA256=07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51
zip=$DIR/wintun-$WINTUN_VERSION.zip
if [ ! -f "$zip" ]; then
    curl -fsSL -o "$zip.tmp" "https://www.wintun.net/builds/wintun-$WINTUN_VERSION.zip"
    mv "$zip.tmp" "$zip"
fi
sum=$( (sha256sum "$zip" 2>/dev/null || shasum -a 256 "$zip") | cut -d' ' -f1)
if [ "$sum" != "$WINTUN_SHA256" ]; then
    rm -f "$zip"
    echo "wintun zip checksum mismatch: got $sum" >&2
    exit 1
fi

mkdir -p "$OUT"
cp "$DIR/$TARGET/release/multilink.exe" multilink.example.toml "$OUT/"
unzip -p "$zip" "wintun/bin/$ARCH/wintun.dll" > "$OUT/wintun.dll"
unzip -p "$zip" wintun/LICENSE.txt > "$OUT/wintun-LICENSE.txt"
echo "Bundle ready: $OUT (run multilink.exe from an Administrator prompt)"
