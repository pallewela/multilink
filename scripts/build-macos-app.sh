#!/bin/sh
# Build the macOS menu bar app: target/Multilink.app
#
#   sh scripts/build-macos-app.sh && open target/Multilink.app
#   SIGN_IDENTITY="Developer ID Application: ..." sh scripts/build-macos-app.sh
#
# Needs: Xcode command line tools (swiftc) and Rust. The multilink library is linked into the
# app (macos/multilink.h). The app runs as you; its privileged half is the same executable run
# by launchd as root (`Multilink daemon`, Contents/Library/LaunchDaemons). The first Start
# registers it; macOS then asks you to allow it once in System Settings > Login Items.
# The daemon runs code from this bundle as root, so install the app where only admins can
# change it (/Applications) before relying on it.
set -eu
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=target/Multilink.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Library/LaunchDaemons"
cargo build --release   # builds the CLI and target/release/libmultilink.a
rm -f "$APP/Contents/MacOS/multilink"   # older builds bundled a separate binary
rm -f "$APP/Contents/Library/LaunchDaemons/com.multilink.bar.daemon.plist"   # old label
# Frameworks and libraries per `cargo rustc --lib --crate-type staticlib -- --print native-static-libs`.
swiftc -O -target "$(uname -m)-apple-macos13" -import-objc-header macos/multilink.h \
    -o "$APP/Contents/MacOS/Multilink" macos/Multilink.swift target/release/libmultilink.a \
    -framework CoreFoundation -framework SystemConfiguration -framework Foundation -lobjc -liconv
# App icon, drawn by macos/icon.swift and scaled to every size an .icns holds.
ICONSET=target/AppIcon.iconset
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
swift macos/icon.swift "$ICONSET/icon_512x512@2x.png"
for s in 16 32 128 256 512; do
    sips -z $s $s "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
    [ $s = 512 ] || sips -z $((s * 2)) $((s * 2)) "$ICONSET/icon_512x512@2x.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.multilink.bar</string>
    <key>CFBundleName</key><string>Multilink</string>
    <key>CFBundleExecutable</key><string>Multilink</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleShortVersionString</key><string>${VERSION%%-*}</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <!-- Menu bar only: no Dock icon, no app menu. -->
    <key>LSUIElement</key><true/>
    <!-- The status page is plain HTTP on localhost. -->
    <key>NSAppTransportSecurity</key>
    <dict><key>NSAllowsLocalNetworking</key><true/></dict>
</dict>
</plist>
EOF
# Started on demand when the app connects; the daemon exits again when idle.
cat > "$APP/Contents/Library/LaunchDaemons/com.multilink.helper.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.multilink.helper</string>
    <key>BundleProgram</key><string>Contents/MacOS/Multilink</string>
    <key>ProgramArguments</key><array><string>Multilink</string><string>daemon</string></array>
    <key>MachServices</key><dict><key>com.multilink.helper</key><true/></dict>
    <key>AssociatedBundleIdentifiers</key><array><string>com.multilink.bar</string></array>
</dict>
</plist>
EOF
# Hardened runtime: no code injection (DYLD_* variables, unsigned libraries) into the app or
# the daemon. Ad-hoc by default; the XPC check pins each build's cdhash either way.
codesign --force --options runtime --sign "${SIGN_IDENTITY:--}" "$APP"
echo "Built $APP (open it with: open $APP)"
