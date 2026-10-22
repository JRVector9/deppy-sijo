#!/bin/sh
# Build/sign the raw developer executables and their assets. Never launches Deppy.
set -eu

cd "$(dirname "$0")/.."

BUNDLE_ID="app.vector9.deppy-sijo"
PROFILE_DIR="debug"
CARGO_ARGS=""
case "$#:${1:-}" in
    0:) ;;
    1:--release) PROFILE_DIR="release"; CARGO_ARGS="--release" ;;
    *) echo "usage: dev-build.sh [--release]" >&2; exit 2 ;;
esac

# Every sibling executable must match the App build, including OpenSSH askpass.
cargo build --locked -p deppy-sijo -p mcp-proxy -p ssh-askpass $CARGO_ARGS
BIN="target/$PROFILE_DIR/deppy-sijo"
TUNNEL_BIN="target/$PROFILE_DIR/deppy-cloudflared"
ASKPASS_BIN="target/$PROFILE_DIR/deppy-ssh-askpass"
python3 scripts/prepare-cloudflared.py --output "$TUNNEL_BIN"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')"
HELPER_ARTIFACTS=${DEPPY_REMOTE_HOST_ARTIFACTS:-target/remote-host/$VERSION}
# Raw target/{debug,release}/deppy-sijo resolves its resources at target/Resources.
# Sealed Linux artifacts must already exist; developer builds never invoke Docker.
if ! python3 scripts/prepare-dev-resources.py --source "$HELPER_ARTIFACTS" --version "$VERSION"; then
    echo "Build the sealed helper pair with scripts/build-remote-host.sh, or set DEPPY_REMOTE_HOST_ARTIFACTS to a matching validated pair." >&2
    exit 1
fi

# Match package-macos.sh's identity preference to retain developer TCC identity.
IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null || true)
pick_identity() {
    echo "$IDENTITIES" | grep -o "\"$1[^\"]*\"" | head -1 | tr -d '"'
}
SIGN_ID="${DEPPY_SIGN_IDENTITY:-}"
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Developer ID Application")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Apple Development")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "deppy-sijo-dev")

if [ -n "$SIGN_ID" ]; then
    codesign --force --sign "$SIGN_ID" -i "$BUNDLE_ID" "$BIN"
    codesign --force --sign "$SIGN_ID" -i "$BUNDLE_ID.tunnel" "$TUNNEL_BIN"
    codesign --force --sign "$SIGN_ID" -i "$BUNDLE_ID.ssh-askpass" "$ASKPASS_BIN"
    echo "서명: $SIGN_ID (identifier=$BUNDLE_ID — 재빌드해도 권한 유지)"
else
    echo "경고: 코드서명 인증서가 없어 ad-hoc으로 둡니다 — 재빌드마다 macOS가"
    echo "      권한(데스크탑 접근·키체인)을 다시 물어봅니다."
    echo "      해결: Xcode 계정에 Apple ID를 추가하거나 Developer ID 인증서를 설치."
fi
