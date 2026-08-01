#!/bin/sh
# 개발용 빌드 + 고정 서명 + 실행.
#
# 왜 필요한가 (2026-07-17 실증):
#   `cargo build`가 만드는 바이너리는 ad-hoc(linker-signed) 서명이라 designated
#   requirement가 `cdhash H"..."` — 바이너리 해시 자체다. 코드를 고치면 해시가 바뀌고
#   macOS는 그걸 **다른 앱**으로 본다. 그래서 재빌드할 때마다 데스크탑 폴더 접근 등
#   TCC 권한을 다시 물어보고, 키체인(env secret)도 매번 암호를 묻는다.
#
#   Apple 발급 인증서로 서명하면 requirement가
#   `identifier "app.vector9.deppy-sijo" and anchor apple generic and ... OU = <team>`
#   이 되어 **코드가 바뀌어도 같은 앱**이다 — 권한을 한 번만 주면 계속 유지된다.
#   (self-signed는 TCC는 고정하지만 키체인 partition에 못 들어가 keyring 접근마다
#   암호를 묻는다 — scripts/package-macos.sh 주석의 2026-07-08 실증 참고.)
#
# 사용: scripts/dev-run.sh [--release] [-- <앱 인자>]
#   DEPPY_SIGN_IDENTITY 로 인증서를 지정할 수 있다(미지정 시 아래 우선순위로 자동 선택).
set -eu

cd "$(dirname "$0")/.."

BUNDLE_ID="app.vector9.deppy-sijo"
PROFILE_DIR="debug"
CARGO_ARGS=""
if [ "${1:-}" = "--release" ]; then
    PROFILE_DIR="release"
    CARGO_ARGS="--release"
    shift
fi
[ "${1:-}" = "--" ] && shift

# mcp-proxy도 함께 빌드한다 — 앱이 자기 실행 파일 옆의 deppy-mcp-proxy를 statusLine/hook
# 커맨드로 그대로 등록하므로(app.rs mcp_proxy_bin), 이걸 빼먹으면 stale 빌드가 계속 실행돼
# 그 바이너리에 새로 추가한 기능(예: Claude usage 기록)이 조용히 반영되지 않는다
# (2026-08-01 실증: target/debug/deppy-mcp-proxy가 7/24 빌드로 멈춰 있어 7/26에 추가한
# claude-usage.json 기록이 며칠째 안 됐다). package-macos.sh와 같은 패턴을 유지한다.
cargo build -p deppy-sijo -p mcp-proxy $CARGO_ARGS
BIN="target/$PROFILE_DIR/deppy-sijo"

# 서명 우선순위는 package-macos.sh와 동일하게 유지한다 — 배포본과 개발본의
# designated requirement가 갈리면 권한이 따로 놀아 같은 문제가 재발한다.
IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null || true)
pick_identity() {
    echo "$IDENTITIES" | grep -o "\"$1[^\"]*\"" | head -1 | tr -d '"'
}
SIGN_ID="${DEPPY_SIGN_IDENTITY:-}"
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Developer ID Application")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "Apple Development")
[ -z "$SIGN_ID" ] && SIGN_ID=$(pick_identity "deppy-sijo-dev")

if [ -n "$SIGN_ID" ]; then
    # -i 로 identifier를 고정한다 — ad-hoc의 기본 identifier(deppy_sijo-<해시>)는
    # 그 자체가 빌드마다 달라져 requirement를 흔든다.
    codesign --force --sign "$SIGN_ID" -i "$BUNDLE_ID" "$BIN"
    echo "서명: $SIGN_ID (identifier=$BUNDLE_ID — 재빌드해도 권한 유지)"
else
    echo "경고: 코드서명 인증서가 없어 ad-hoc으로 둡니다 — 재빌드마다 macOS가"
    echo "      권한(데스크탑 접근·키체인)을 다시 물어봅니다."
    echo "      해결: Xcode 계정에 Apple ID를 추가하거나 Developer ID 인증서를 설치."
fi

exec "./$BIN" "$@"
