#!/bin/sh
# Deppy Relay 신뢰 셸 빌드 — 불변 아티팩트 하나와 그 사이드카 매니페스트 하나를 만든다.
#
#   sh web/relay-shell/build.sh          # 빌드
#   sh web/relay-shell/build.sh verify   # 다이제스트 대조(변조 검출)
#
# 필수 환경변수 — **기본값은 없다**. 기본값이 있는 순간 그것이 곧 잘못된 오리진이다.
#   SHELL_ORIGIN   신뢰 셸 오리진 (https://...)
#   RELAY_ORIGIN   신뢰하지 않는 데이터 평면 오리진 (wss://...)
# 선택:
#   RELAY_SHELL_DIST   산출물 디렉터리 (기본: 이 스크립트 옆의 dist)
#   GIT_REVISION       git이 없는 환경에서 리비전을 명시할 때만
#
# 배포는 여전히 **BLOCKED**다(deploy/relay/README.md의 상태표). 이 스크립트는 아티팩트만
# 만든다 — 도메인·DNS·TLS·레지스트리 소유자가 정해지기 전까지 publish는 존재하지 않는다.

set -eu

SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
DIST=${RELAY_SHELL_DIST:-"$SRC/dist"}
MANIFEST_NAME=relay-shell.manifest.json
FILES="index.html relay-shell.js relay-terminal.js relay-crypto.js relay-shell.css sw.js manifest.webmanifest"
# web/shared에서 가져오는 자산. 셸 문서가 직접 참조하므로 **필수**이며, 빠지면 빌드가 멈춘다.
SHARED_FILES="mobile-theme.css"
CRYPTO_MODULE=relay-crypto.js
SHARED_DIR=$SRC/../shared

die() {
    printf 'relay-shell build: %s\n' "$1" >&2
    exit 1
}

warn() {
    printf 'relay-shell build: WARNING: %s\n' "$1" >&2
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | sed 's/.*= //'
    else
        die 'no SHA-256 tool found (sha256sum, shasum, or openssl required)'
    fi
}

json_field() {
    # $1 파일, $2 키. 첫 일치만 쓴다 — 매니페스트는 키가 유일한 평평한 문서다.
    sed -n "s/^[[:space:]]*\"$2\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" "$1" | head -n 1
}

host_of() {
    printf '%s' "$1" | sed -e 's|^[a-z][a-z0-9+.-]*://||' -e 's|[:/].*$||'
}

# RFC 2606/6761 예약 이름이면 자리표시자다. 실제 도메인이 정해지면 저절로 false가 된다.
is_placeholder_host() {
    case "$1" in
        *.invalid | *.example | *.test | *.localhost | example.com | *.example.com) return 0 ;;
        *) return 1 ;;
    esac
}

# ── verify ──────────────────────────────────────────────────────────────────────────

verify() {
    manifest=$DIST/$MANIFEST_NAME
    [ -f "$manifest" ] || die "manifest not found: $manifest"

    archive_name=$(json_field "$manifest" archive)
    archive_digest=$(json_field "$manifest" archive_sha256)
    [ -n "$archive_name" ] || die 'manifest has no archive name'
    [ -n "$archive_digest" ] || die 'manifest has no archive digest'
    [ -f "$DIST/$archive_name" ] || die "archive not found: $DIST/$archive_name"

    # 아티팩트 이름 자체가 내용 주소다. 이름과 다이제스트가 갈라지면 그건 다른 릴리스다.
    [ "$archive_name" = "relay-shell-$archive_digest.tar.gz" ] ||
        die "archive name does not address its own digest: $archive_name"

    actual=$(sha256_of "$DIST/$archive_name")
    [ "$actual" = "$archive_digest" ] ||
        die "archive digest mismatch: manifest $archive_digest, actual $actual"

    work=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$work'" EXIT INT TERM
    tar -xzf "$DIST/$archive_name" -C "$work" || die 'archive could not be extracted'
    [ -d "$work/relay-shell" ] || die 'archive does not contain a relay-shell directory'

    for name in $FILES $SHARED_FILES; do
        recorded=$(json_field "$manifest" "$name")
        [ -n "$recorded" ] || die "manifest has no digest for $name"
        [ -f "$work/relay-shell/$name" ] || die "archive is missing $name"
        found=$(sha256_of "$work/relay-shell/$name")
        [ "$found" = "$recorded" ] ||
            die "digest mismatch for $name: manifest $recorded, archive $found"
    done

    crypto_recorded=$(json_field "$manifest" crypto_module_sha256)
    crypto_found=$(sha256_of "$work/relay-shell/$CRYPTO_MODULE")
    [ "$crypto_recorded" = "$crypto_found" ] ||
        die "crypto module digest mismatch: manifest $crypto_recorded, archive $crypto_found"

    # 아카이브에 매니페스트가 모르는 파일이 섞여 있으면 그건 검증된 릴리스가 아니다.
    for present in "$work"/relay-shell/*; do
        base=$(basename "$present")
        case " $FILES $SHARED_FILES " in
            *" $base "*) ;;
            *) die "archive contains an unrecorded file: $base" ;;
        esac
    done

    printf 'relay-shell verify: OK (%s)\n' "$archive_name"
    printf 'relay-shell verify: deployment remains BLOCKED — artifact only\n'
}

# ── build ───────────────────────────────────────────────────────────────────────────

build() {
    [ -n "${SHELL_ORIGIN:-}" ] || die 'SHELL_ORIGIN is required and has no default'
    [ -n "${RELAY_ORIGIN:-}" ] || die 'RELAY_ORIGIN is required and has no default'

    case "$SHELL_ORIGIN" in
        https://*[!/]) ;;
        *) die "SHELL_ORIGIN must be https://<host>[:port] with no trailing slash: $SHELL_ORIGIN" ;;
    esac
    case "$RELAY_ORIGIN" in
        wss://*[!/]) ;;
        *) die "RELAY_ORIGIN must be wss://<host>[:port] with no trailing slash: $RELAY_ORIGIN" ;;
    esac

    if [ -n "${GIT_REVISION:-}" ]; then
        revision=$GIT_REVISION
    elif revision=$(git -C "$SRC" rev-parse HEAD 2>/dev/null); then
        :
    else
        die 'git rev-parse HEAD failed and GIT_REVISION is unset'
    fi

    protocol_version=$(sed -n 's/^export const PROTOCOL_VERSION = \([0-9][0-9]*\);$/\1/p' \
        "$SRC/$CRYPTO_MODULE")
    [ -n "$protocol_version" ] || die 'PROTOCOL_VERSION not found in relay-crypto.js'
    min_protocol_version=$(sed -n 's/^export const MIN_PROTOCOL_VERSION = \([0-9][0-9]*\);$/\1/p' \
        "$SRC/relay-shell.js")
    [ -n "$min_protocol_version" ] || die 'MIN_PROTOCOL_VERSION not found in relay-shell.js'

    work=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$work'" EXIT INT TERM
    stage=$work/relay-shell
    mkdir -p "$stage"

    # 1) 자산을 옮기며 오리진 자리표시자를 릴리스 상수로 바꾼다. 원본 트리는 건드리지 않는다.
    for name in $FILES; do
        [ -f "$SRC/$name" ] || die "missing source asset: $name"
        sed -e "s|__SHELL_ORIGIN__|$SHELL_ORIGIN|g" -e "s|__RELAY_ORIGIN__|$RELAY_ORIGIN|g" \
            "$SRC/$name" >"$stage/$name"
    done

    # 2) 공용 자산(web/shared). 셸 문서가 직접 참조하므로 없으면 아티팩트가 성립하지 않는다.
    for name in $SHARED_FILES; do
        [ -f "$SHARED_DIR/$name" ] || die "missing shared asset: $name"
        cp "$SHARED_DIR/$name" "$stage/$name"
    done

    # 3) 셸 버전 = sw.js를 뺀 모든 자산 다이제스트의 해시. sw.js가 자기 버전을 담으므로
    #    순환을 피하려면 sw.js는 계산에서 빠져야 한다.
    version_input=$work/version-input
    : >"$version_input"
    for name in $FILES $SHARED_FILES; do
        [ "$name" = sw.js ] && continue
        printf '%s  %s\n' "$(sha256_of "$stage/$name")" "$name" >>"$version_input"
    done
    sort "$version_input" >"$version_input.sorted"
    shell_version=$(sha256_of "$version_input.sorted" | cut -c1-16)
    sed "s|__SHELL_VERSION__|$shell_version|g" "$SRC/sw.js" >"$stage/sw.js"

    # 4) 아카이브. 이름이 곧 내용 주소다.
    (cd "$work" && tar -cf - relay-shell) | gzip -9 -n >"$work/archive.tar.gz"
    archive_digest=$(sha256_of "$work/archive.tar.gz")
    archive_name=relay-shell-$archive_digest.tar.gz

    mkdir -p "$DIST"
    rm -f "$DIST"/relay-shell-*.tar.gz
    cp "$work/archive.tar.gz" "$DIST/$archive_name"

    shell_host=$(host_of "$SHELL_ORIGIN")
    relay_host=$(host_of "$RELAY_ORIGIN")
    shell_placeholder=false
    relay_placeholder=false
    is_placeholder_host "$shell_host" && shell_placeholder=true
    is_placeholder_host "$relay_host" && relay_placeholder=true

    csp="default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src $RELAY_ORIGIN; manifest-src 'self'; worker-src 'self'; base-uri 'none'; object-src 'none'; form-action 'none'; frame-ancestors 'none'; require-trusted-types-for 'script'; upgrade-insecure-requests"

    manifest=$DIST/$MANIFEST_NAME
    {
        printf '{\n'
        printf '  "schema": "deppy-relay-shell-manifest/v1",\n'
        printf '  "generated_by": "web/relay-shell/build.sh",\n'
        printf '  "git_revision": "%s",\n' "$revision"
        printf '  "protocol_version": %s,\n' "$protocol_version"
        printf '  "min_protocol_version": %s,\n' "$min_protocol_version"
        printf '  "shell_origin": "%s",\n' "$SHELL_ORIGIN"
        printf '  "shell_origin_is_placeholder": %s,\n' "$shell_placeholder"
        printf '  "relay_origin": "%s",\n' "$RELAY_ORIGIN"
        printf '  "relay_origin_is_placeholder": %s,\n' "$relay_placeholder"
        printf '  "deployment_status": "BLOCKED",\n'
        printf '  "deployment_blockers": [\n'
        printf '    "shell DNS owner and exact hostname",\n'
        printf '    "shell TLS edge and certificate issuance/renewal owner",\n'
        printf '    "immutable artifact registry / CDN and its access control",\n'
        printf '    "GitHub environment and secret names",\n'
        printf '    "relay data-plane coordinates (see deploy/relay/README.md)"\n'
        printf '  ],\n'
        printf '  "shell_version": "%s",\n' "$shell_version"
        printf '  "archive": "%s",\n' "$archive_name"
        printf '  "archive_sha256": "%s",\n' "$archive_digest"
        printf '  "crypto_module": "%s",\n' "$CRYPTO_MODULE"
        printf '  "crypto_module_sha256": "%s",\n' "$(sha256_of "$stage/$CRYPTO_MODULE")"
        printf '  "content_security_policy": "%s",\n' "$csp"
        printf '  "files": {\n'
        first=1
        for name in $FILES $SHARED_FILES; do
            [ "$first" = 1 ] || printf ',\n'
            first=0
            printf '    "%s": "%s"' "$name" "$(sha256_of "$stage/$name")"
        done
        printf '\n  }\n'
        printf '}\n'
    } >"$manifest"

    printf 'relay-shell build: %s\n' "$DIST/$archive_name"
    printf 'relay-shell build: %s\n' "$manifest"
    printf 'relay-shell build: shell_version=%s\n' "$shell_version"
    printf 'relay-shell build: deployment is BLOCKED — this is an artifact, not a release\n'
}

case "${1:-build}" in
    build) build ;;
    verify) verify ;;
    *) die "unknown mode: $1 (expected build or verify)" ;;
esac
