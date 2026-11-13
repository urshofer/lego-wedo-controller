#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) macOS app bundle: target/macos/WeDo Controller.app
# plus a zipped copy for distribution.
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="WeDo Controller"
BIN="wedo"
BUNDLE_ID="ch.rokfor.wedo"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
OUT="target/macos"
APP="$OUT/$APP_NAME.app"

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

for t in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --bin "$BIN" --target "$t"
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

lipo -create \
  "target/aarch64-apple-darwin/release/$BIN" \
  "target/x86_64-apple-darwin/release/$BIN" \
  -output "$APP/Contents/MacOS/$BIN"

if [ -f assets/AppIcon.icns ]; then
  cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSInputMonitoringUsageDescription</key><string>Reads game controller input to drive the LEGO WeDo hub.</string>
</dict>
</plist>
EOF

# Ad-hoc signature (required to run on Apple Silicon). Set SIGN_ID to a
# "Developer ID Application: ..." identity for distribution.
codesign --force --deep --options runtime --sign "${SIGN_ID:--}" "$APP"

( cd "$OUT" && rm -f "$APP_NAME-$VERSION-macos.zip" &&ditto -c -k --keepParent "$APP_NAME.app" "$APP_NAME-$VERSION-macos.zip" )

echo "Built: $APP"
echo "Zip:   $OUT/$APP_NAME-$VERSION-macos.zip"
