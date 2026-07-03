#!/bin/sh
# PR-20: macOS app bundle 생성 (설계문서 PR-20 — 기본안: 수동 macOS bundle).
# release 바이너리를 .app 구조로 감싼다. 서명/공증은 배포 단계 소관(후속).
#
# 사용: scripts/package-macos.sh
# 산출: target/bundle/Deppy Sijo.app
set -eu

cd "$(dirname "$0")/.."

APP_NAME="Deppy Sijo"
BIN_NAME="deppy-sijo"
BUNDLE_ID="app.vector9.deppy-sijo"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"

cargo build --release

BUNDLE="target/bundle/$APP_NAME.app"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

cp "target/release/$BIN_NAME" "$BUNDLE/Contents/MacOS/$BIN_NAME"

cat > "$BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleName</key><string>$APP_NAME</string>
    <key>CFBundleDisplayName</key><string>$APP_NAME</string>
    <key>CFBundleExecutable</key><string>$BIN_NAME</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# ad-hoc 서명 — 미서명 바이너리는 최신 macOS에서 실행이 막힐 수 있다
codesign --force --sign - "$BUNDLE"

echo "bundle: $BUNDLE"
