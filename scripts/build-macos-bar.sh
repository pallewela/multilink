#!/bin/sh
# Build the macOS menu bar app: target/MultilinkBar.app
#
#   sh scripts/build-macos-bar.sh && open target/MultilinkBar.app
#   SIGN_IDENTITY="Developer ID Application: ..." sh scripts/build-macos-bar.sh
#
# Needs: Xcode command line tools (swiftc) and Rust. The multilink library is linked into the
# app (macos/multilink.h). The app runs as you; its privileged half is the same executable run
# by launchd as root (`MultilinkBar daemon`, Contents/Library/LaunchDaemons). The first Start
# registers it; macOS then asks you to allow it once in System Settings > Login Items.
# The daemon runs code from this bundle as root, so install the app where only admins can
# change it (/Applications) before relying on it.
set -eu
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=target/MultilinkBar.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Library/LaunchDaemons"
cargo build --release   # builds the CLI and target/release/libmultilink.a
rm -f "$APP/Contents/MacOS/multilink"   # older builds bundled a separate binary
# Frameworks and libraries per `cargo rustc --lib --crate-type staticlib -- --print native-static-libs`.
swiftc -O -target "$(uname -m)-apple-macos13" -import-objc-header macos/multilink.h \
    -o "$APP/Contents/MacOS/MultilinkBar" macos/MultilinkBar.swift target/release/libmultilink.a \
    -framework CoreFoundation -framework SystemConfiguration -framework Foundation -lobjc -liconv
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.multilink.bar</string>
    <key>CFBundleName</key><string>MultilinkBar</string>
    <key>CFBundleExecutable</key><string>MultilinkBar</string>
    <key>CFBundlePackageType</key><string>APPL</string>
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
cat > "$APP/Contents/Library/LaunchDaemons/com.multilink.bar.daemon.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.multilink.bar.daemon</string>
    <key>BundleProgram</key><string>Contents/MacOS/MultilinkBar</string>
    <key>ProgramArguments</key><array><string>MultilinkBar</string><string>daemon</string></array>
    <key>MachServices</key><dict><key>com.multilink.bar.daemon</key><true/></dict>
    <key>AssociatedBundleIdentifiers</key><array><string>com.multilink.bar</string></array>
</dict>
</plist>
EOF
# Hardened runtime: no code injection (DYLD_* variables, unsigned libraries) into the app or
# the daemon. Ad-hoc by default; the XPC check pins each build's cdhash either way.
codesign --force --options runtime --sign "${SIGN_IDENTITY:--}" "$APP"
echo "Built $APP (open it with: open $APP)"
