#!/bin/sh
# PR-20: macOS app bundle 생성 (설계문서 PR-20 — 기본안: 수동 macOS bundle).
# release 바이너리를 .app 구조로 감싼다. 서명/공증은 배포 단계 소관(후속).
#
# 사용(배포, 기본 fail-closed): scripts/package-macos.sh
# 사용(명시적 로컬 개발): DEPPY_REQUIRE_TRUSTED_SIGNING=0 \
#   DEPPY_ALLOW_UNTRUSTED_SIGNING=1 scripts/package-macos.sh
# 산출: target/bundle/Deppy Sijo.app, target/bundle/Deppy Sijo.zip
set -eu

cd "$(dirname "$0")/.."

APP_NAME="Deppy Sijo"
BIN_NAME="deppy-sijo"
BUNDLE_ID="app.vector9.deppy-sijo"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"

# Resolve and validate signing policy before the expensive release build. Production packaging is
# fail-closed; local untrusted output requires an explicit two-variable opt-in.
IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null || true)
pick_identity() {
    echo "$IDENTITIES" | grep -o "\"$1[^\"]*\"" | head -1 | tr -d '"'
}
SIGN_ID="${DEPPY_SIGN_IDENTITY:-}"
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Developer ID Application")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Apple Development")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "deppy-sijo-dev")
REQUIRE_TRUSTED=${DEPPY_REQUIRE_TRUSTED_SIGNING:-1}
ALLOW_UNTRUSTED=${DEPPY_ALLOW_UNTRUSTED_SIGNING:-0}
case "$REQUIRE_TRUSTED:$ALLOW_UNTRUSTED" in
    1:0 | 1:1 | 0:1) ;;
    *)
        echo "untrusted package requires DEPPY_ALLOW_UNTRUSTED_SIGNING=1" >&2
        exit 1
        ;;
esac
if [ "$REQUIRE_TRUSTED" = "1" ]; then
    case "$SIGN_ID" in
        "Developer ID Application:"*) ;;
        *)
            echo "production package requires a Developer ID Application identity" >&2
            exit 1
            ;;
    esac
fi

cargo build --release -p deppy-sijo -p mcp-proxy

BUNDLE="target/bundle/$APP_NAME.app"
ARCHIVE="target/bundle/$APP_NAME.zip"
rm -rf "$BUNDLE"
rm -f "$ARCHIVE"
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
if [ -n "$SIGN_ID" ]; then
    case "$SIGN_ID" in
        "Developer ID Application:"*)
            for binary in "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
                codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$binary"
            done
            codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$BUNDLE"
            ;;
        *)
            for binary in "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
                codesign --force --sign "$SIGN_ID" "$binary"
            done
            codesign --force --sign "$SIGN_ID" "$BUNDLE"
            ;;
    esac
    echo "서명: $SIGN_ID (고정 identity — TCC/키체인 권한 재빌드 후 유지)"
else
    for binary in "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
        codesign --force --sign - "$binary"
    done
    codesign --force --sign - "$BUNDLE"
    echo "서명: ad-hoc — 재빌드마다 macOS 권한(데스크탑 접근 등)을 다시 물어봅니다."
    echo "  한 번만 설정하려면: sh scripts/setup-dev-signing.sh (비밀번호 1회)"
fi

ditto -c -k --sequesterRsrc --keepParent "$BUNDLE" "$ARCHIVE"
sh scripts/verify-macos-package.sh "$BUNDLE" "$ARCHIVE"

echo "bundle: $BUNDLE"
echo "archive: $ARCHIVE"
