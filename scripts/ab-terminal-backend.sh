#!/bin/bash
# 터미널 백엔드 A/B 실측 인프라 — alacritty vs libghostty-vt (2026-07-11 실측 기준).
# LibGhosttyBackend는 feature `ghostty-backend` 뒤 experimental — 기본 빌드 무관.
#
# 사용:
#   scripts/ab-terminal-backend.sh build   # feature 빌드 + ReleaseFast dylib 준비
#   scripts/ab-terminal-backend.sh run     # ghostty 백엔드로 앱 실행 (수동 확인)
#   scripts/ab-terminal-backend.sh ab      # 하네스 A/B 자동 측정 (~3분, 결과 표 출력)
#
# 이 스크립트가 우회하는 빌드 함정 3종 (제거 조건 포함):
#  1) zig 0.15.2는 macOS 26.4/Xcode 26.4 SDK에서 네이티브 링크 불가
#     (SDK가 libSystem.tbd의 arm64-macos 슬라이스 제거 — ziglang #31658).
#     → fake xcrun이 SDK 질의에 CLT의 15.x SDK를 반환. ghostty 핀 소스가
#       zig 0.16으로 올라가면 fake xcrun/zig 다운로드 모두 불필요.
#  2) libghostty-vt-sys 0.1.1 build.rs가 -Doptimize를 안 넘겨 Debug dylib 생성
#     (verifyIntegrity로 CPU 35배). → vendored 소스에서 ReleaseFast 재빌드.
#  3) dylib에 rpath가 없어 실행 시 DYLD_LIBRARY_PATH 필요.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/deppy-sijo"
TOOL="$ROOT/target/ab-toolchain"
ZIG_VERSION=0.15.2
ARCH=$(uname -m | sed 's/arm64/aarch64/')
ZIG_DIR="$TOOL/zig-$ARCH-macos-$ZIG_VERSION"

setup_toolchain() {
  mkdir -p "$TOOL"
  if [ ! -x "$ZIG_DIR/zig" ]; then
    echo ">> zig $ZIG_VERSION 다운로드 (ghostty 핀 버전 — 시스템 zig와 무관)"
    curl -fsSL -o "$TOOL/zig.tar.xz" \
      "https://ziglang.org/download/$ZIG_VERSION/zig-$ARCH-macos-$ZIG_VERSION.tar.xz"
    tar xf "$TOOL/zig.tar.xz" -C "$TOOL" && rm "$TOOL/zig.tar.xz"
  fi
  # fake xcrun: 함정 1) 우회 — 슬라이스가 온전한 CLT 15.x SDK를 돌려준다
  SDK=$(ls -d /Library/Developer/CommandLineTools/SDKs/MacOSX15*.sdk 2>/dev/null | tail -1)
  if [ -z "$SDK" ]; then
    echo "!! CLT에 MacOSX15.x SDK가 없습니다 — zig 0.15가 링크할 SDK가 필요합니다." >&2
    echo "   (ghostty가 zig 0.16+로 올라갔다면 이 우회 자체가 불필요 — 스크립트 헤더 참조)" >&2
    exit 1
  fi
  mkdir -p "$TOOL/fake-bin"
  cat > "$TOOL/fake-bin/xcrun" <<EOF
#!/bin/sh
case "\$*" in
  *--show-sdk-path*) echo $SDK ;;
  *--show-sdk-version*) echo 15.4 ;;
  *) exec /usr/bin/xcrun "\$@" ;;
esac
EOF
  chmod +x "$TOOL/fake-bin/xcrun"
  export PATH="$TOOL/fake-bin:$ZIG_DIR:$PATH"
}

build() {
  setup_toolchain
  echo ">> release 빌드 (feature ghostty-backend)"
  cargo build --release -p deppy-sijo --features ghostty-backend
  # 함정 2) 우회: Debug dylib을 ReleaseFast로 재빌드해 교체
  SRC=$(find "$ROOT/target/release/build" -path "*out/ghostty-src" -type d | head -1)
  INSTALL="$(dirname "$SRC")/ghostty-install"
  echo ">> libghostty-vt ReleaseFast 재빌드 (Debug 기본값 교체 — 함정 2)"
  (cd "$SRC" && zig build -Demit-lib-vt -Doptimize=ReleaseFast --prefix "$INSTALL")
  echo ">> 완료. dylib: $INSTALL/lib"
}

libdir() {
  find "$ROOT/target/release/build" -path "*ghostty-install/lib" -type d | head -1
}

run_app() {
  local backend=$1; shift || true
  env DEPPY_TERM_BACKEND="$backend" DYLD_LIBRARY_PATH="$(libdir)" "$@" "$BIN"
}

measure() { # measure <backend> — 하네스로 90s 측정, 결과 한 줄 요약
  local backend=$1
  local home_iso="$TOOL/ab-home-$backend"
  rm -rf "$home_iso"; mkdir -p "$home_iso"
  env HOME="$home_iso" DEPPY_PERF_HARNESS=1 DEPPY_FRAME_STATS=1 \
    DEPPY_TERM_BACKEND="$backend" DYLD_LIBRARY_PATH="$(libdir)" \
    "$BIN" > "$TOOL/ab-$backend.stderr" 2>&1 &
  local pid=$!
  sleep 20 # 워밍업
  if ! ps -p $pid > /dev/null; then
    echo "!! [$backend] 앱 조기 종료:"; tail -5 "$TOOL/ab-$backend.stderr"; return 1
  fi
  local t0 w0 t1 w1 rss
  t0=$(cputime_cs $pid); w0=$(date +%s)
  sleep 60
  t1=$(cputime_cs $pid); w1=$(date +%s)
  sleep 5; rss=$(ps -o rss= -p $pid | tr -d ' ')
  kill $pid 2>/dev/null; sleep 1; kill -9 $pid 2>/dev/null || true
  local cpu ghostty_count log p95
  cpu=$(python3 -c "print(f'{($t1 - $t0) / 100 / ($w1 - $w0) * 100:.1f}')")
  ghostty_count=$(grep -c "backend=ghostty" "$TOOL/ab-$backend.stderr" || true)
  log=$(find "$home_iso" -name "app.log*" 2>/dev/null | head -1)
  p95=$(grep "frame stats" "$log" 2>/dev/null | tail -6 | grep -o "p95_ms=[0-9.]*" | cut -d= -f2 \
        | sort -n | tail -1 | cut -c1-5)
  echo "[$backend] CPU(60s): ${cpu}%  RSS(85s): $((rss / 1024))MB  frame_p95(max): ${p95:-?}ms  ghostty세션: $ghostty_count"
}

cputime_cs() {
  python3 - "$1" <<'EOF'
import subprocess, sys
out = subprocess.run(["ps", "-o", "cputime=", "-p", sys.argv[1]], capture_output=True, text=True).stdout.strip()
secs = 0.0
for p in out.replace("-", ":").split(":"): secs = secs * 60 + float(p or 0)
print(int(secs * 100))
EOF
}

case "${1:-}" in
  build) build ;;
  run)
    [ -x "$BIN" ] || { echo "먼저 build를 실행하세요"; exit 1; }
    run_app ghostty ;;
  ab)
    [ -x "$BIN" ] || build
    echo "== A/B 측정 시작 (백엔드당 ~90s, 하네스: 세션 11개/hidden 10/대량출력 3)"
    measure alacritty
    measure ghostty
    echo "== 참고: 2026-07-11 실측 — CPU 동급(3.0~4.5 vs 2.7~4.3%), RSS ghostty ~5-10% 낮음" ;;
  *) sed -n '2,10p' "$0"; exit 1 ;;
esac
