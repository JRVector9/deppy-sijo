#!/bin/sh
# PR-20: macOS app bundle 생성 (설계문서 PR-20 — 기본안: 수동 macOS bundle).
# release 바이너리를 .app 구조로 감싸고, production 산출물은 Developer ID 서명 후
# Apple 공증과 ticket staple까지 완료한다.
#
# 사용(배포, 기본 fail-closed): scripts/package-macos.sh
#   DEPPY_NOTARY_KEYCHAIN_PROFILE 또는 DEPPY_NOTARY_KEY + DEPPY_NOTARY_KEY_ID
#   (Team API key는 DEPPY_NOTARY_ISSUER도 함께 지정)가 필요하다.
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
NOTARY_PROFILE=${DEPPY_NOTARY_KEYCHAIN_PROFILE:-}
NOTARY_KEY=${DEPPY_NOTARY_KEY:-}
NOTARY_KEY_ID=${DEPPY_NOTARY_KEY_ID:-}
NOTARY_ISSUER=${DEPPY_NOTARY_ISSUER:-}
NOTARY_TIMEOUT=${DEPPY_NOTARY_TIMEOUT:-20m}
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
    if [ -n "$NOTARY_PROFILE" ]; then
        if [ -n "$NOTARY_KEY$NOTARY_KEY_ID$NOTARY_ISSUER" ]; then
            echo "choose either DEPPY_NOTARY_KEYCHAIN_PROFILE or API-key notarization variables" >&2
            exit 1
        fi
    else
        if [ -z "$NOTARY_KEY" ] || [ -z "$NOTARY_KEY_ID" ]; then
            echo "production package requires DEPPY_NOTARY_KEYCHAIN_PROFILE or DEPPY_NOTARY_KEY + DEPPY_NOTARY_KEY_ID" >&2
            exit 1
        fi
        if [ ! -f "$NOTARY_KEY" ]; then
            echo "notary API key does not exist: $NOTARY_KEY" >&2
            exit 1
        fi
    fi
fi

cargo build --release -p deppy-sijo -p mcp-proxy
python3 scripts/prepare-cloudflared.py --output target/release/deppy-cloudflared

BUNDLE="target/bundle/$APP_NAME.app"
ARCHIVE="target/bundle/$APP_NAME.zip"
rm -rf "$BUNDLE"
rm -f "$ARCHIVE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

cp "target/release/$BIN_NAME" "$BUNDLE/Contents/MacOS/$BIN_NAME"
# agent-proxy 브리지 바이너리를 앱 옆에 함께 동봉한다 — mcp_proxy_bin()이 실행 파일
# 옆에서 찾으므로, 권한계층 경유 스폰이 프록시를 확실히 찾게 한다 (codex).
cp "target/release/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/deppy-mcp-proxy"
cp "target/release/deppy-cloudflared" "$BUNDLE/Contents/MacOS/deppy-cloudflared"
cp scripts/vendor/cloudflared-LICENSE "$BUNDLE/Contents/Resources/cloudflared-LICENSE"

# 배포 바이너리에서만 심볼 테이블을 제거해 용량을 줄인다. target/release의 원본은
# 그대로 두어 디버깅(lldb 스택 트레이스 등)에는 계속 심볼 있는 바이너리를 쓸 수 있다.
strip -x "$BUNDLE/Contents/MacOS/$BIN_NAME"
strip -x "$BUNDLE/Contents/MacOS/deppy-mcp-proxy"

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
            for binary in "$BUNDLE/Contents/MacOS/deppy-cloudflared" "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
                codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$binary"
            done
            codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$BUNDLE"
            ;;
        *)
            for binary in "$BUNDLE/Contents/MacOS/deppy-cloudflared" "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
                codesign --force --sign "$SIGN_ID" "$binary"
            done
            codesign --force --sign "$SIGN_ID" "$BUNDLE"
            ;;
    esac
    echo "서명: $SIGN_ID (고정 identity — TCC/키체인 권한 재빌드 후 유지)"
else
    for binary in "$BUNDLE/Contents/MacOS/deppy-cloudflared" "$BUNDLE/Contents/MacOS/deppy-mcp-proxy" "$BUNDLE/Contents/MacOS/$BIN_NAME"; do
        codesign --force --sign - "$binary"
    done
    codesign --force --sign - "$BUNDLE"
    echo "서명: ad-hoc — 재빌드마다 macOS 권한(데스크탑 접근 등)을 다시 물어봅니다."
    echo "  한 번만 설정하려면: sh scripts/setup-dev-signing.sh (비밀번호 1회)"
fi

ditto -c -k --sequesterRsrc --keepParent "$BUNDLE" "$ARCHIVE"

if [ "$REQUIRE_TRUSTED" = "1" ]; then
    NOTARY_RESULT=$(mktemp /tmp/deppy-notary-result.XXXXXX)
    trap 'rm -f "$NOTARY_RESULT"' EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    NOTARY_SUBMIT_EXIT=0
    if [ -n "$NOTARY_PROFILE" ]; then
        xcrun notarytool submit "$ARCHIVE" \
            --wait --timeout "$NOTARY_TIMEOUT" --output-format plist \
            --keychain-profile "$NOTARY_PROFILE" >"$NOTARY_RESULT" || NOTARY_SUBMIT_EXIT=$?
    elif [ -n "$NOTARY_ISSUER" ]; then
        xcrun notarytool submit "$ARCHIVE" \
            --wait --timeout "$NOTARY_TIMEOUT" --output-format plist \
            --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" \
            >"$NOTARY_RESULT" || NOTARY_SUBMIT_EXIT=$?
    else
        xcrun notarytool submit "$ARCHIVE" \
            --wait --timeout "$NOTARY_TIMEOUT" --output-format plist \
            --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" >"$NOTARY_RESULT" || NOTARY_SUBMIT_EXIT=$?
    fi
    if [ "$NOTARY_SUBMIT_EXIT" -ne 0 ]; then
        echo "notarization request failed: exit=$NOTARY_SUBMIT_EXIT" >&2
        /usr/bin/plutil -p "$NOTARY_RESULT" >&2 || true
        exit "$NOTARY_SUBMIT_EXIT"
    fi
    NOTARY_STATUS=$(/usr/bin/plutil -extract status raw -o - "$NOTARY_RESULT" 2>/dev/null || true)
    if [ "$NOTARY_STATUS" != "Accepted" ]; then
        echo "notarization failed: status=${NOTARY_STATUS:-missing}" >&2
        /usr/bin/plutil -p "$NOTARY_RESULT" >&2 || true
        exit 1
    fi
    NOTARY_ID=$(/usr/bin/plutil -extract id raw -o - "$NOTARY_RESULT" 2>/dev/null || true)
    echo "공증: Accepted (${NOTARY_ID:-submission id unavailable})"
    xcrun stapler staple "$BUNDLE"

    # 업로드용 ZIP은 staple 전에 만들었다. ticket을 포함한 배포 ZIP으로 다시 만든다.
    rm -f "$ARCHIVE"
    ditto -c -k --sequesterRsrc --keepParent "$BUNDLE" "$ARCHIVE"
fi

sh scripts/verify-macos-package.sh "$BUNDLE" "$ARCHIVE"

echo "bundle: $BUNDLE"
echo "archive: $ARCHIVE"
