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
# agent-proxy 브리지 바이너리를 앱 옆에 함께 동봉한다 — mcp_proxy_bin()이 실행 파일
# 옆에서 찾으므로, 권한계층 경유 스폰이 프록시를 확실히 찾게 한다 (codex).
cp "target/release/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/deppy-mcp-proxy"

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

# 서명 — scripts/setup-dev-signing.sh로 신뢰 설정한 고정 인증서가 있으면 그것으로 서명한다.
# ad-hoc(`--sign -`)은 매 빌드 cdhash가 달라져 macOS TCC(데스크탑 폴더 접근 등)가 매번 앱을
# "새 앱"으로 보고 권한을 재요청한다. 고정 인증서로 서명하면 한 번 승인한 권한이 유지된다.
CERT_CN="deppy-sijo-dev"
if security find-identity -v -p codesigning 2>/dev/null | grep -q "$CERT_CN"; then
    codesign --force --deep --sign "$CERT_CN" "$BUNDLE"
    echo "서명: $CERT_CN (고정 identity — TCC 권한 재빌드 후 유지)"
else
    codesign --force --sign - "$BUNDLE"
    echo "서명: ad-hoc — 재빌드마다 macOS 권한(데스크탑 접근 등)을 다시 물어봅니다."
    echo "  한 번만 설정하려면: sh scripts/setup-dev-signing.sh (비밀번호 1회)"
fi

echo "bundle: $BUNDLE"
