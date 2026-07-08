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

# 서명 — Apple 발급 인증서(Developer ID/Apple Development)가 있으면 최우선으로 쓴다.
# self-signed(deppy-sijo-dev)는 TCC(폴더 접근)는 고정하지만 Apple 신뢰 체인이 아니라
# **키체인 partition**에 못 들어가, keyring 항목(env secret) 접근마다 로그인 키체인
# 암호를 물어본다(빌드마다 ~30회, 2026-07-08 실증). Apple 인증서 서명이면 '항상 허용'이
# designated requirement(identifier+cert)로 유지돼 재빌드 후에도 재프롬프트가 없다.
# 우선순위: $DEPPY_SIGN_IDENTITY > Developer ID Application > Apple Development >
#           deppy-sijo-dev(self-signed) > ad-hoc.
IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null)
pick_identity() {
    echo "$IDENTITIES" | grep -o "\"$1[^\"]*\"" | head -1 | tr -d '"'
}
SIGN_ID="${DEPPY_SIGN_IDENTITY:-}"
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Developer ID Application")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Apple Development")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "deppy-sijo-dev")
if [ -n "$SIGN_ID" ]; then
    codesign --force --deep --sign "$SIGN_ID" "$BUNDLE"
    echo "서명: $SIGN_ID (고정 identity — TCC/키체인 권한 재빌드 후 유지)"
else
    codesign --force --sign - "$BUNDLE"
    echo "서명: ad-hoc — 재빌드마다 macOS 권한(데스크탑 접근 등)을 다시 물어봅니다."
    echo "  한 번만 설정하려면: sh scripts/setup-dev-signing.sh (비밀번호 1회)"
fi

echo "bundle: $BUNDLE"
