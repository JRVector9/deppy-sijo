#!/bin/bash
# 렌더러 A/B 벤치 러너·집계 하네스 — glow(OpenGL) vs wgpu(Metal). PR-04 기준선/비교용.
# 앱 측 계측(JSONL 이벤트)은 앱이 담당한다. 이 스크립트는 **실행·수집·집계만** 한다.
#
# 사용:
#   scripts/render-bench.sh build                        # release 빌드(+ bench-alloc 변형)
#   scripts/render-bench.sh run <renderer> <scenario> [ws]  # 단일 실행
#   scripts/render-bench.sh matrix                       # 전체 매트릭스 직렬 실행 (~30분)
#   scripts/render-bench.sh report                       # JSONL+CSV → summary.md / summary.csv
#   scripts/render-bench.sh -h                           # 상세 도움말(시나리오·환경변수·정책)
#
# 측정 오염 방지 3원칙 (이 스크립트가 강제한다):
#  1) Debug 빌드로 성능 결론 금지 — build가 만든 release 사본만 실행한다.
#  2) 동시 실행 금지 — 매트릭스는 **직렬**, 실행 간 5초 안정화(SETTLE_SECS).
#  3) 사용자 실데이터 오염 금지 — 실행마다 격리 HOME(target/render-bench/home-*)을 쓰고,
#     실제 data dir(~/Library/Application Support/app.vector9.deppy-sijo)의 변경을 감시한다.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HELPERS="$ROOT/scripts/render-bench"
ART="$ROOT/artifacts/render-bench"          # 산출물(JSONL/CSV/summary) — git 정책은 ART/.gitignore
WORK="$ROOT/target/render-bench"            # 실행 사본 + 격리 HOME (target은 이미 gitignore)
BIN_PLAIN="$WORK/bin/deppy-sijo"            # release 사본
BIN_ALLOC="$WORK/bin/deppy-sijo-alloc"      # release + --features bench-alloc 사본
REAL_DATA_DIR="$HOME/Library/Application Support/app.vector9.deppy-sijo"

RENDERERS="glow wgpu"
SCENARIOS="idle dirty1 bulk fullscreen switch createdelete selection agenttui"
SAMPLE_INTERVAL="${SAMPLE_INTERVAL:-1}"     # 외부 샘플러 간격(초)
SETTLE_SECS="${SETTLE_SECS:-5}"             # 실행 간 안정화 대기
BENCH_ITERS="${DEPPY_BENCH_ITERS:-100}"     # createdelete 반복 횟수
USE_HARNESS="${USE_HARNESS:-0}"             # 1이면 기존 DEPPY_PERF_HARNESS 부하도 함께 켠다

die() { echo "!! $*" >&2; exit 1; }
note() { echo ">> $*"; }

usage() {
  sed -n '2,16p' "$0"
  cat <<'EOF'

시나리오 (DEPPY_BENCH_SCENARIO):
  idle          정지 상태. idle CPU / 워크스페이스 스케일 RSS 측정 (ws 1·5·10·20)
  dirty1        1행만 갱신. dirty 렌더 경로 frame p95
  bulk          대량 출력. bulk frame p95 / CPU
  fullscreen    전체 화면 갱신. 최악 케이스 frame p95
  switch        워크스페이스 전환 반복. 전환 latency
  createdelete  워크스페이스 생성·삭제 반복(기본 100회). RSS 기울기 → 누수 판정
  agenttui      에이전트 TUI 근사(alt screen + 스피너 10Hz + 부분 갱신 + 스트리밍). 실제 핫패스
  selection     **자동화 불가 — 수동 절차**. 드래그 선택을 사람이 수행한다(아래 참조)

매트릭스 (조합 폭발 방지 — 실제로 도는 것만):
  {glow,wgpu} × idle          × ws {1,5,10,20}   ← 스케일 테스트
  {glow,wgpu} × bulk          × ws {1,5,10,20}   ← 스케일 테스트
  {glow,wgpu} × dirty1        × ws 1
  {glow,wgpu} × fullscreen    × ws 1
  {glow,wgpu} × switch        × ws 5             (전환은 ws≥2 필요)
  {glow,wgpu} × createdelete  × ws 1 (iters=100)
  {glow,wgpu} × agenttui      × ws 1
  selection 은 매트릭스에서 제외 — `run <renderer> selection` 로 수동 실행.

환경변수(스크립트 → 앱):
  DEPPY_RENDERER, DEPPY_RENDER_BENCH=1, DEPPY_BENCH_OUT, DEPPY_BENCH_SCENARIO,
  DEPPY_BENCH_WORKSPACES, DEPPY_BENCH_SECS, DEPPY_BENCH_ITERS,
  DEPPY_FRAME_STATS=1, DEPPY_RESOURCE_STATS=1,
  DEPPY_ALLOC_STATS=1 (bench-alloc 빌드에서만), DEPPY_PERF_HARNESS=1 (USE_HARNESS=1일 때만)

스크립트 튜닝 변수:
  SECS=<n>            시나리오 기본 시간 덮어쓰기      SAMPLE_INTERVAL=<n>  외부 샘플링 간격(기본 1s)
  SETTLE_SECS=<n>     실행 간 안정화(기본 5s)          DEPPY_BENCH_ITERS=<n> createdelete 반복(기본 100)
  USE_HARNESS=1       기존 부하 하네스도 함께 실행     ALLOC=1              alloc 계측 빌드로 실행

수동 절차 — selection (자동화 불가):
  1) scripts/render-bench.sh run glow selection      # 앱이 뜨고 SECS(기본 60s) 동안 유지된다
  2) 뜬 창의 터미널 영역에서 60초 동안 **드래그 선택을 반복**한다(위/아래 가장자리까지 끌어
     오토스크롤도 유발). 더블/트리플 클릭도 섞는다.
  3) 앱이 스스로 종료되면 artifacts/render-bench/<renderer>/selection-ws1.jsonl 이 남는다.
  4) wgpu도 동일 반복. 미실행이면 report가 UNKNOWN(수동 미실행)으로 표기한다.

git 커밋 정책:
  원시 JSONL/CSV/stderr 는 용량이 커서 **커밋하지 않는다**(artifacts/render-bench/.gitignore).
  summary.md / summary.csv / screenshots/** 만 추적한다.
EOF
}

# ── 검증 헬퍼 ────────────────────────────────────────────────────────────────
valid_in() { # valid_in <값> <공백구분 허용목록>
  local v=$1 x
  for x in $2; do
    [ "$v" = "$x" ] && return 0
  done
  return 1
}

has_bench_alloc_feature() {
  grep -qE '^bench-alloc[[:space:]]*=' "$ROOT/crates/app/Cargo.toml"
}

default_secs() { # 시나리오별 기본 실행 시간(초)
  case "$1" in
    idle) echo 60 ;;
    dirty1|bulk|fullscreen|switch|agenttui) echo 30 ;;
    createdelete) echo 300 ;;   # 100회 반복 상한 — 앱이 먼저 끝나면 조기 종료
    selection) echo 60 ;;
    *) echo 30 ;;
  esac
}

# ── build ───────────────────────────────────────────────────────────────────
# release만 만든다. Debug 바이너리는 이 스크립트가 쓰지 않는다(성능 결론 금지).
build() {
  mkdir -p "$WORK/bin"
  note "release 벤치 빌드 (--features render-glow — glow/wgpu A/B 전용)"
  (cd "$ROOT" && cargo build --release -p deppy-sijo --features render-glow)
  cp "$ROOT/target/release/deppy-sijo" "$BIN_PLAIN"

  if has_bench_alloc_feature; then
    note "release 빌드 (--features render-glow,bench-alloc — A/B + frame allocation)"
    (cd "$ROOT" && cargo build --release -p deppy-sijo --features render-glow,bench-alloc)
    cp "$ROOT/target/release/deppy-sijo" "$BIN_ALLOC"
    # 기본 사본이 alloc 빌드로 덮이지 않도록 plain을 다시 만든다(같은 산출물 경로 공유).
    (cd "$ROOT" && cargo build --release -p deppy-sijo --features render-glow)
    cp "$ROOT/target/release/deppy-sijo" "$BIN_PLAIN"
  else
    rm -f "$BIN_ALLOC"
    echo "!! cargo feature 'bench-alloc' 없음 (crates/app/Cargo.toml) — ALLOC=1 실행 불가."
    echo "   frame alloc count/bytes 는 report에서 UNKNOWN(bench-alloc 미구현)으로 표기된다."
  fi
  note "완료: $WORK/bin"
}

# ── 실행 전 방어 점검 ────────────────────────────────────────────────────────
preflight() { # preflight <바이너리>
  local bin=$1
  [ -x "$bin" ] || die "실행 바이너리 없음: $bin — 먼저 'scripts/render-bench.sh build'"
  case "$bin" in
    "$WORK/bin/"*) : ;;
    *) die "release 사본만 실행한다(Debug 성능 결론 금지): $bin" ;;
  esac
  # 다른 deppy-sijo 인스턴스가 떠 있으면 측정이 오염된다(LockFile 충돌 + CPU 경합).
  if pgrep -x deppy-sijo > /dev/null 2>&1; then
    die "deppy-sijo가 이미 실행 중이다 — 측정 오염/락 충돌. 종료 후 다시 실행하라."
  fi
}

data_dir_stamp() { # 실데이터 오염 감시용 mtime 스냅샷
  [ -d "$REAL_DATA_DIR" ] && stat -f %m "$REAL_DATA_DIR" || echo none
}

# ── run ─────────────────────────────────────────────────────────────────────
run_one() { # run_one <renderer> <scenario> [ws]
  local renderer=$1 scenario=$2 ws=${3:-1}
  valid_in "$renderer" "$RENDERERS" || die "renderer는 {$RENDERERS} 중 하나: $renderer"
  valid_in "$scenario" "$SCENARIOS" || die "scenario는 {$SCENARIOS} 중 하나: $scenario"
  case "$ws" in ''|*[!0-9]*) die "ws는 양의 정수: $ws" ;; esac

  local bin="$BIN_PLAIN" alloc=0
  if [ "${ALLOC:-0}" = "1" ]; then
    [ -x "$BIN_ALLOC" ] || die "alloc 빌드 없음 — cargo feature 'bench-alloc' 구현 후 build 재실행"
    bin="$BIN_ALLOC"; alloc=1
  fi
  preflight "$bin"

  local secs; secs=$(default_secs "$scenario")
  secs="${SECS:-$secs}"
  # selection은 앱 계약에 없는 시나리오다 — idle을 베이스로 띄우고 사람이 드래그한다.
  local app_scenario="$scenario" manual=0
  if [ "$scenario" = "selection" ]; then app_scenario=idle; manual=1; ws=1; fi

  local tag="$scenario-ws$ws"
  local outdir="$ART/$renderer"
  mkdir -p "$outdir"
  local jsonl="$outdir/$tag.jsonl" csv="$outdir/samples-$tag.csv"
  local meta="$outdir/meta-$tag.json" errlog="$outdir/stderr-$tag.log"
  rm -f "$jsonl" "$csv" "$meta" "$errlog"

  local home_iso="$WORK/home-$renderer-$tag"
  rm -rf "$home_iso"; mkdir -p "$home_iso"
  local stamp_before; stamp_before=$(data_dir_stamp)

  note "[$renderer/$tag] 시작 — secs=$secs ws=$ws alloc=$alloc harness=$USE_HARNESS"
  if [ "$manual" = 1 ]; then
    echo "   ** 수동 시나리오 **: 창이 뜨면 ${secs}초 동안 터미널 영역에서 드래그 선택을"
    echo "      반복하라(가장자리까지 끌어 오토스크롤 유발 + 더블/트리플 클릭 혼합)."
  fi

  # 실행 — GUI 창이 뜬다. 직렬 실행 전제(동시 실행 시 측정 오염).
  # 격리 HOME → directories::ProjectDirs가 $HOME 아래를 쓰므로 실데이터와 분리된다.
  local t_start; t_start=$(date +%s)
  (
    export HOME="$home_iso"
    export DEPPY_RENDERER="$renderer"
    export DEPPY_RENDER_BENCH=1
    export DEPPY_BENCH_OUT="$jsonl"
    export DEPPY_BENCH_SCENARIO="$app_scenario"
    export DEPPY_BENCH_WORKSPACES="$ws"
    export DEPPY_BENCH_SECS="$secs"
    export DEPPY_BENCH_ITERS="$BENCH_ITERS"
    export DEPPY_FRAME_STATS=1
    export DEPPY_RESOURCE_STATS=1
    if [ "$alloc" = 1 ]; then export DEPPY_ALLOC_STATS=1; fi
    if [ "$USE_HARNESS" = "1" ]; then export DEPPY_PERF_HARNESS=1; fi
    exec "$bin" > "$errlog" 2>&1
  ) &
  local pid=$!

  python3 "$HELPERS/sample.py" --pid "$pid" --out "$csv" --interval "$SAMPLE_INTERVAL" &
  local spid=$!

  # 종료 대기 — 앱은 DEPPY_BENCH_SECS 후 스스로 끝난다. 안 끝나면 grace 후 강제 종료.
  local grace=60
  [ "$scenario" = createdelete ] && grace=180
  local deadline=$(( t_start + secs + grace ))
  local status=ok
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      status=timeout
      echo "!! [$renderer/$tag] ${secs}s+${grace}s 안에 자체 종료하지 않음 — 강제 종료(계약 위반 가능)"
      kill "$pid" 2>/dev/null || true; sleep 3; kill -9 "$pid" 2>/dev/null || true
      break
    fi
    sleep 1
  done
  local code=0
  wait "$pid" 2>/dev/null || code=$?
  kill "$spid" 2>/dev/null || true
  wait "$spid" 2>/dev/null || true
  local elapsed=$(( $(date +%s) - t_start ))

  # ── 계약 검증 (조용한 통과 금지) ──
  if [ "$elapsed" -lt 3 ] && [ "$status" = ok ]; then
    echo "--- stderr (마지막 20줄) ---"; tail -20 "$errlog" >&2 || true
    die "[$renderer/$tag] 앱이 ${elapsed}s만에 종료 — 실행 실패(렌더러 미지원/크래시?)"
  fi
  if [ ! -s "$jsonl" ]; then
    echo "--- stderr (마지막 20줄) ---"; tail -20 "$errlog" >&2 || true
    die "[$renderer/$tag] JSONL 미생성/빈 파일: $jsonl
   → 앱 계측 계약(DEPPY_RENDER_BENCH/DEPPY_BENCH_OUT) 미구현이거나 이벤트를 쓰지 않았다."
  fi
  grep -qE '"ev"[[:space:]]*:[[:space:]]*"renderer"' "$jsonl" \
    || die "[$renderer/$tag] JSONL에 renderer 이벤트 없음 — 계약 미구현: $jsonl"
  grep -qE "\"backend\"[[:space:]]*:[[:space:]]*\"$renderer\"" "$jsonl" \
    || echo "!! [$renderer/$tag] 경고: renderer 이벤트의 backend가 '$renderer'가 아니다 — DEPPY_RENDERER 무시 여부 확인"

  # ── 실데이터 오염 방어 점검 ──
  if [ ! -d "$home_iso/Library/Application Support/app.vector9.deppy-sijo" ]; then
    echo "!! [$renderer/$tag] 경고: 격리 HOME에 앱 data dir이 안 생겼다 — 앱이 실제 data dir을 썼을 수 있다."
  fi
  if [ "$(data_dir_stamp)" != "$stamp_before" ]; then
    echo "!! [$renderer/$tag] 경고: 실제 data dir이 변경되었다($REAL_DATA_DIR) — 벤치가 사용자 데이터를 건드렸다."
  fi

  cat > "$meta" <<EOF
{"renderer":"$renderer","scenario":"$scenario","app_scenario":"$app_scenario","ws":$ws,
 "secs":$secs,"iters":$BENCH_ITERS,"alloc_build":$alloc,"harness":${USE_HARNESS:-0},"manual":$manual,
 "status":"$status","exit_code":$code,"elapsed_s":$elapsed,"sample_interval_s":$SAMPLE_INTERVAL,
 "started_unix":$t_start,"binary":"$bin"}
EOF
  note "[$renderer/$tag] 완료 — status=$status exit=$code elapsed=${elapsed}s → $jsonl"
}

# ── matrix ──────────────────────────────────────────────────────────────────
# 직렬 실행. 각 실행 사이 SETTLE_SECS 안정화(GPU/캐시/썸 잔열 배제).
matrix() {
  # 렌더러당 13회 = idle×4(ws) + bulk×4(ws) + dirty1 + fullscreen + switch + createdelete + agenttui
  note "매트릭스 26회 직렬 실행 — 예상 25~40분. 실행 중 Mac을 건드리지 마라(측정 오염)."
  for r in $RENDERERS; do
    for ws in 1 5 10 20; do
      run_one "$r" idle "$ws"; sleep "$SETTLE_SECS"
      run_one "$r" bulk "$ws"; sleep "$SETTLE_SECS"
    done
    run_one "$r" dirty1 1;       sleep "$SETTLE_SECS"
    run_one "$r" fullscreen 1;   sleep "$SETTLE_SECS"
    run_one "$r" switch 5;       sleep "$SETTLE_SECS"
    run_one "$r" createdelete 1; sleep "$SETTLE_SECS"
    run_one "$r" agenttui 1;     sleep "$SETTLE_SECS"
  done
  note "매트릭스 완료 — selection(수동)은 별도 실행 후 report"
  report
}

# ── report ──────────────────────────────────────────────────────────────────
report() {
  [ -d "$ART" ] || die "산출물 디렉터리 없음: $ART — 먼저 run/matrix"
  python3 "$HELPERS/report.py" --art "$ART" \
    || die "집계 실패 — $ART 의 JSONL/CSV 확인"
  note "집계 완료: $ART/summary.md, $ART/summary.csv"
}

case "${1:-}" in
  build) build ;;
  run)
    [ $# -ge 3 ] || { usage; die "run <renderer> <scenario> [ws]"; }
    run_one "$2" "$3" "${4:-1}" ;;
  matrix) matrix ;;
  report) report ;;
  -h|--help|help) usage ;;
  *) usage; exit 1 ;;
esac
