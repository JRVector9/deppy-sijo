#!/bin/sh
# 개발용 Relay 종단 환경 — 이 Mac 하나에서 데이터 평면과 모바일 셸을 모두 띄운다.
#
#   sh scripts/relay-dev.sh up      # relay-server + 셸을 tailnet https/wss로 올린다
#   sh scripts/relay-dev.sh env     # 앱에 줄 환경변수만 출력한다
#   sh scripts/relay-dev.sh app     # 그 환경변수로 dev-run.sh를 실행한다
#   sh scripts/relay-dev.sh status  # 지금 상태
#   sh scripts/relay-dev.sh down    # 서버 정지 + serve 설정 회수
#
# 왜 Tailscale인가: Mac 클라이언트는 `wss://` + **DNS 이름**만 받는다(ws://·IP 리터럴·
# localhost는 정책으로 거부, 디버그 빌드에서도 마찬가지다). `tailscale serve`는 이 노드에
# 유효한 인증서와 `*.ts.net` 이름을 이미 주므로, 도메인을 사지 않고도 그 정책을 만족한다.
# serve는 **tailnet 전용**이다 — 폰이 Tailscale에 들어와 있어야 붙는다. 공개 인터넷 노출은
# 하지 않는다(그건 `funnel`이고, 이 스크립트는 쓰지 않는다).
#
# macOS의 Tailscale은 샌드박스 때문에 디렉터리를 직접 서비스하지 못한다(실측: "Path serving is
# not supported on macOS"). 그래서 셸은 loopback 정적 서버에 올리고 serve가 그것을 프록시한다.
#
# 이것은 배포가 아니다. `deploy/relay/README.md`의 좌표는 여전히 BLOCKED이며, 여기서 만드는
# 라우트/자격증명은 이 기계 안에서만 쓰는 1회성 개발값이다.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

# 기존 모바일 웹이 443을 쓰고 있으므로 겹치지 않는 포트를 쓴다.
RELAY_PORT=${RELAY_PORT:-8443}
SHELL_PORT=${SHELL_PORT:-10000}
RELAY_LOCAL=${RELAY_LOCAL:-127.0.0.1:9443}
SHELL_LOCAL_PORT=${SHELL_LOCAL_PORT:-10080}
# 라우트/자격증명은 추적하지 않는 파일에 남긴다 — 서버와 앱이 같은 값을 봐야 하기 때문이다.
ENV_FILE=$ROOT/.relay-dev.env
SERVE_DIR=$ROOT/target/relay-shell-serve
LOG=${TMPDIR:-/tmp}/deppy-relay-server.log
SHELL_LOG=${TMPDIR:-/tmp}/deppy-relay-shell-http.log

TS=${TAILSCALE_BIN:-/Applications/Tailscale.app/Contents/MacOS/Tailscale}
[ -x "$TS" ] || TS=$(command -v tailscale || true)

die() {
    printf 'relay-dev: %s\n' "$1" >&2
    exit 1
}

need_tailscale() {
    [ -n "$TS" ] && [ -x "$TS" ] || die 'tailscale CLI를 찾을 수 없다 (TAILSCALE_BIN으로 지정)'
}

# 이 노드의 tailnet DNS 이름. 인증서가 이 이름으로 발급돼 있어야 한다.
ts_host() {
    need_tailscale
    "$TS" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))'
}

# 라우트 16바이트 + 승인 자격증명 32바이트. 기본값은 존재하지 않는다 — 매번 난수로 만들고
# 그 값을 파일에 남겨 서버와 앱이 같은 것을 보게 한다.
load_or_create_env() {
    if [ ! -f "$ENV_FILE" ]; then
        route=$(head -c 16 /dev/urandom | xxd -p | tr -d '\n')
        credential=$(head -c 32 /dev/urandom | xxd -p | tr -d '\n')
        (
        umask 077
        cat >"$ENV_FILE" <<EOF
# 개발용 Relay 라우트/자격증명 (git이 추적하지 않는다). 지우면 다음 up에서 새로 만들어진다.
DEPPY_RELAY_DEV_ROUTE=$route
DEPPY_RELAY_DEV_ADMISSION=$credential
EOF
        )
        printf 'relay-dev: 새 라우트/자격증명을 %s 에 만들었다\n' "$ENV_FILE"
    fi
    # shellcheck disable=SC1090
    . "$ENV_FILE"
    [ ${#DEPPY_RELAY_DEV_ROUTE} -eq 32 ] || die 'route 길이가 32 hex가 아니다'
    [ ${#DEPPY_RELAY_DEV_ADMISSION} -eq 64 ] || die 'admission 길이가 64 hex가 아니다'
}

origins() {
    host=$(ts_host)
    RELAY_ORIGIN=wss://$host:$RELAY_PORT
    SHELL_ORIGIN=https://$host:$SHELL_PORT
}

up() {
    load_or_create_env
    origins

    # 1) 데이터 평면. 평문 TCP 위 WebSocket만 말한다 — TLS는 tailscale serve가 종단한다.
    cargo build -p relay-server
    pkill -f 'target/debug/relay-server' 2>/dev/null || true
    DEPPY_RELAY_BIND=$RELAY_LOCAL \
    DEPPY_RELAY_ROUTES="$DEPPY_RELAY_DEV_ROUTE:$DEPPY_RELAY_DEV_ADMISSION" \
        nohup "$ROOT/target/debug/relay-server" >"$LOG" 2>&1 </dev/null &
    sleep 1
    pgrep -f 'target/debug/relay-server' >/dev/null || {
        cat "$LOG" >&2
        die 'relay-server가 뜨지 않았다'
    }

    # 2) 모바일 셸 아티팩트. 오리진 자리표시자가 여기서 실제 이름으로 바뀐다.
    SHELL_ORIGIN=$SHELL_ORIGIN RELAY_ORIGIN=$RELAY_ORIGIN sh web/relay-shell/build.sh
    archive=$(ls -t web/relay-shell/dist/relay-shell-*.tar.gz | head -1)
    rm -rf "$SERVE_DIR"
    mkdir -p "$SERVE_DIR"
    tar -xzf "$archive" -C "$SERVE_DIR" --strip-components=1

    # 3) 셸을 loopback 정적 서버에 올린다(macOS serve는 디렉터리를 직접 못 준다).
    pkill -f "http.server $SHELL_LOCAL_PORT" 2>/dev/null || true
    (cd "$SERVE_DIR" && nohup python3 -m http.server "$SHELL_LOCAL_PORT" --bind 127.0.0.1 \
        >"$SHELL_LOG" 2>&1 </dev/null &)
    sleep 1
    curl -sf -o /dev/null "http://127.0.0.1:$SHELL_LOCAL_PORT/index.html" || {
        cat "$SHELL_LOG" >&2
        die "셸 정적 서버가 127.0.0.1:$SHELL_LOCAL_PORT 에 뜨지 않았다"
    }

    # 4) tailnet https/wss 이름 부여. serve는 tailnet 전용이다(공개 노출 아님).
    need_tailscale
    "$TS" serve --bg --https="$RELAY_PORT" "$RELAY_LOCAL" >/dev/null 2>&1 </dev/null
    "$TS" serve --bg --https="$SHELL_PORT" "127.0.0.1:$SHELL_LOCAL_PORT" >/dev/null 2>&1 </dev/null

    printf '\nrelay-dev: 준비됨\n'
    printf '  relay  %s  ← %s\n' "$RELAY_ORIGIN" "$RELAY_LOCAL"
    printf '  shell  %s  ← 127.0.0.1:%s (%s)\n' "$SHELL_ORIGIN" "$SHELL_LOCAL_PORT" "$SERVE_DIR"
    printf '  log    %s\n' "$LOG"
    printf '\n앱 실행:  sh scripts/relay-dev.sh app\n'
}

env_lines() {
    load_or_create_env
    origins
    printf 'DEPPY_RELAY_DEV_ENDPOINT=%s\n' "$RELAY_ORIGIN"
    printf 'DEPPY_RELAY_DEV_SHELL_ORIGIN=%s\n' "$SHELL_ORIGIN"
    printf 'DEPPY_RELAY_DEV_ROUTE=%s\n' "$DEPPY_RELAY_DEV_ROUTE"
    # 승인 자격증명은 마스킹한다 — 이 값이 라우트 소유권이며 스크롤백·히스토리에 남으면 안 된다.
    printf 'DEPPY_RELAY_DEV_ADMISSION=%s… (%s 참고)\n' \
        "$(printf '%s' "$DEPPY_RELAY_DEV_ADMISSION" | cut -c1-8)" "$ENV_FILE"
}

app() {
    load_or_create_env
    origins
    pkill -x deppy-sijo 2>/dev/null || true
    sleep 1
    DEPPY_RELAY_DEV_ENDPOINT=$RELAY_ORIGIN \
    DEPPY_RELAY_DEV_SHELL_ORIGIN=$SHELL_ORIGIN \
    DEPPY_RELAY_DEV_ROUTE=$DEPPY_RELAY_DEV_ROUTE \
    DEPPY_RELAY_DEV_ADMISSION=$DEPPY_RELAY_DEV_ADMISSION \
        exec sh scripts/dev-run.sh
}

status() {
    if pgrep -f 'target/debug/relay-server' >/dev/null; then
        printf 'relay-server: 실행 중 (pid %s)\n' \
            "$(pgrep -f 'target/debug/relay-server' | tr '\n' ' ')"
    else
        printf 'relay-server: 정지\n'
    fi
    if pgrep -f "http.server $SHELL_LOCAL_PORT" >/dev/null; then
        printf '셸 정적 서버: 실행 중 (127.0.0.1:%s)\n' "$SHELL_LOCAL_PORT"
    else
        printf '셸 정적 서버: 정지\n'
    fi
    need_tailscale
    "$TS" serve status
}

down() {
    pkill -f 'target/debug/relay-server' 2>/dev/null || true
    pkill -f "http.server $SHELL_LOCAL_PORT" 2>/dev/null || true
    need_tailscale
    # 이 스크립트가 연 두 포트만 회수한다 — 기존 모바일 웹(443)은 건드리지 않는다.
    "$TS" serve --https="$RELAY_PORT" off >/dev/null 2>&1 || true
    "$TS" serve --https="$SHELL_PORT" off >/dev/null 2>&1 || true
    printf 'relay-dev: 정지 (443의 모바일 웹 serve는 그대로 둔다)\n'
}

case "${1:-up}" in
    up) up ;;
    env) env_lines ;;
    app) app ;;
    status) status ;;
    down) down ;;
    *) die "알 수 없는 명령: $1 (up|env|app|status|down)" ;;
esac
