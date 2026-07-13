# 렌더러 리소스·성능 실측 보고서 (Glow vs Wgpu/Metal)

측정일: 2026-07-14
머신: Apple M2 (macOS, Retina 2x), release 빌드, 전원 연결, 창 1200×800 고정
기준 커밋: `6c92cfa`(계측 인프라) — 수정 전 기준선 / `e794c28`(렌더 낭비 2건 수정) — 수정 후
실행: `scripts/render-bench.sh matrix` — 24회 직렬 + createdelete(300s), 실행 간 5초 안정화
계측: 앱 내부 JSONL(`DEPPY_RENDER_BENCH=1`) + 외부 `ps` 샘플링(교차 검증)

---

## 0. 전제 정정 — **앱은 이미 wgpu/Metal로 돌고 있었다**

과제의 전제("현재 = Glow/OpenGL")는 **틀렸다**. eframe 0.35의 default feature에
`wgpu`가 있고 `glow`는 없다(`eframe-0.35.0/Cargo.toml`). 우리 `Cargo.toml`은 features를
지정하지 않았으므로 **기본 빌드가 곧 wgpu 경로**였다. Cargo.lock에 glow/glutin이 보이는 것은
feature-독립 기재이며 컴파일되지 않았다.

- 실측 확인: `DEPPY_RENDERER=wgpu` → adapter **Apple M2 / backend Metal**.
  `DEPPY_RENDERER=glow` → **OpenGL 4.1 Metal - 90.5**(Apple의 GL-over-Metal 에뮬레이션).
- glow feature를 추가로 켜도 `Renderer::default()`는 wgpu다(eframe `epi.rs:608-612`) →
  **기본 실행 경로 불변**(회귀 없음).
- 따라서 **PR-04는 "전환"이 아니라 "검증·문서화"**이며, 이 보고서가 그 검증이다.
- 감사 문서(`terminal-current-state-audit.md`)의 "현재 glow 확정" 서술은 교정했다.

---

## 1. 요약 표 (수정 전 기준선, ws1 · wgpu가 현재 기본 경로)

| 항목 | Glow/OpenGL | Wgpu/Metal | 차이 | 판정 |
|---|---:|---:|---:|---|
| renderer 초기화 | 153 ms | 185 ms | +32 ms | 무의미(1회성) |
| 시작 RSS | 9 MB | 9 MB | 0 | 동일 |
| renderer 초기화 후 RSS | 114 MB | 116 MB | +2 MB | 동일 |
| first_frame peak RSS | 197 MB | 196 MB | −1 MB | 동일 |
| idle stable RSS (ws1) | 127 MB | 147 MB | +20 MB | 오차 범위(런 편차 ±20MB) |
| bulk stable RSS (ws1) | 178 MB | 174 MB | −4 MB | 동일 |
| ws5 / ws10 / ws20 RSS (bulk) | 236 / 357 / 365 MB | 252 / 307 / 376 MB | — | **동일 추세** |
| workspace당 RSS 증가 | ≈ +10 MB/ws | ≈ +10 MB/ws | 0 | 선형, 렌더러 무관 |
| idle p95 | 0.45 ms | 0.48 ms | +0.03 | 동일 |
| dirty1 p95 | 0.51 ms | **0.30 ms** | **−41%** | wgpu 우세 |
| fullscreen p95 | 0.66 ms | 0.74 ms | +0.08 | 동일 |
| bulk p95 | 0.73 ms | 0.88 ms | +0.15 | 동일 |
| **전 시나리오 p95** | **≤ 0.77 ms** | **≤ 0.88 ms** | — | **예산(16ms)의 5% 미만** |
| idle 60초 프레임 수 | 282~482 | 212~356 | glow가 **1.3~1.5배 많음** | wgpu 우세 |
| 생성·삭제 100회 RSS 기울기 | **−0.039 MB/샘플** | **+0.009 MB/샘플** | ≈ 0 | **누수 징후 없음** |
| GPU buffer/texture 총량 | UNKNOWN | UNKNOWN | — | 사유는 §4 |
| Retina 품질 | UNKNOWN | UNKNOWN | — | 미실시(§5) |
| frame allocation | UNKNOWN(미실행) | 2,823 alloc / 238 KB (dirty1, B1 측정) | — | §3-B 미달 |

**결론(성능·메모리)**: 두 렌더러는 **실질적으로 동등**하며, wgpu가 dirty1 p95와 idle 프레임
수에서 소폭 우세하다. 어느 쪽도 성능·메모리 병목이 아니다.

---

## 2. 실측이 잡아낸 결함 2건 — **정적 분석·코드 감사는 둘 다 KEEP 판정했다**

### 2.1 stale `dirty_ranges` → 같은 스냅샷 repaint마다 전 행 재-shaping

`row_is_dirty()`가 보는 `snapshot.dirty_ranges`는 "**그 스냅샷이 만들어질 때** 바뀐 행"이지
"지난 draw 이후 바뀐 행"이 아니다. 새 출력이 없는 repaint(리소스 표시 갱신 ~2회/초,
pane 플래시 애니메이션, 프레임 페이싱으로 같은 스냅샷 재사용)마다 **같은 행을 계속
재-shaping**했다.

수정: 스냅샷 세대(`snapshot_gen`) 도입 — 같은 세대면 dirty는 이미 소비된 것으로 본다.

| 시나리오 (wgpu, ws1, 30s) | rows_rebuilt/frame 전 | 후 | 감소 |
|---|---:|---:|---:|
| idle | 57.6 | **0.7** | **−99%** |
| bulk | 57.5 | **7.4** | **−87%** |
| dirty1 | 1.1 | 0.9 | (이미 정상) |

bulk p95도 0.88 → **0.66 ms**로 개선됐다. dirty1에서 캐시는 원래 정상 동작했으므로
(rows_rebuilt=1) "캐시가 완전히 죽어 있었다"는 아니다 — **같은 스냅샷을 다시 그릴 때만**
무효화됐고, 그 비중이 idle 100%·bulk 87%였다.

### 2.2 선택 하이라이트가 셀마다 rect 발행

배경색은 `push_bg_run`으로 연속 셀을 run 병합하는데, 선택 하이라이트만 셀 단위였다.
정적 분석(B3, 실빌드 계측): 200×60 **전체 선택** 시 shape 2,252 → 12,062(18배),
발행+테셀레이션 0.33 → **2.23 ms**(프레임 예산의 13%).

수정: 연속 선택 셀을 run으로 병합(배경색과 동일 관례). 회귀 테스트 추가.

---

## 3. 워크스페이스 확장성

| ws 수 | glow bulk RSS | wgpu bulk RSS |
|---:|---:|---:|
| 1 | 178 MB | 174 MB |
| 5 | 236 MB | 252 MB |
| 10 | 357 MB | 307 MB |
| 20 | 365 MB | 376 MB |

- workspace당 **≈ +10 MB**, 렌더러와 무관 — 증가분은 grid·scrollback·PTY·자식 프로세스다.
- **비활성 workspace는 렌더되지 않는다**(§14 정책) — ws20에서도 프레임 시간이 ws1과 동일
  (bulk p95 0.74 ms vs 0.73 ms). GPU draw는 활성 1개 몫뿐이다.
- 워크스페이스마다 Device/Queue/FontSystem/Atlas가 생기지 않는다(egui Context 1개).
- **한계**: `DEPPY_BENCH_WORKSPACES`는 warm 상한 clamp 때문에 실효 9개(active 1 + warm 8).
  ws10·ws20 수치는 그 상한 하에서의 값이다.

## 4. 생성·삭제 100회 (누수 판정)

| | 시작 RSS | 종료 RSS | 최대 | 후반 50% 기울기 | 판정 |
|---|---:|---:|---:|---:|---|
| glow | 202 MB | 91 MB | 202 MB | **−0.039 MB/샘플** | 누수 징후 없음 |
| wgpu | 179 MB | 99 MB | 179 MB | **+0.009 MB/샘플** | 누수 징후 없음 |

RSS가 **감소**하며 끝났고 기울기는 0에 수렴한다. GPU 객체 누적 여부는 UNKNOWN(§5).

## 5. UNKNOWN 항목과 사유

| 항목 | 사유 |
|---|---|
| GPU buffer/texture 총 bytes, upload bytes | wgpu·glow 모두 실제 GPU 할당량을 API로 노출하지 않고, 정점/인덱스 버퍼는 **epaint 백엔드가 내부 관리**해 앱이 descriptor조차 만들지 않는다. 앱이 만드는 텍스처는 폰트 atlas + QR뿐이다(추정치 지어내지 않음) |
| Retina 육안 품질 비교 | 두 백엔드 모두 동일한 egui/epaint 텍스트 경로를 쓰므로 이론상 동일. **캡처·육안 비교 미실시**(절차는 `docs/render-path-analysis.md` §품질 캡처 체크리스트) |
| selection drag frame p95 | 드래그 자동화 불가 — 수동 절차만 제공 |
| frame allocation (glow) | `bench-alloc` 빌드로 wgpu만 측정(2,823 alloc/238 KB per frame, dirty1). glow 측은 미실행 — 렌더러와 무관한 UI 스레드 alloc이라 동일할 것으로 추정하나 **측정하지 않았다** |
| Instruments (Metal System Trace / Allocations) | 미실시 |

## 6. 계측 인프라 (재현 방법)

```bash
scripts/render-bench.sh build                    # release(+bench-alloc)
scripts/render-bench.sh run wgpu idle 5          # 단일
scripts/render-bench.sh matrix                   # 24회 직렬 (~30분)
```
환경변수: `DEPPY_RENDERER=glow|wgpu`, `DEPPY_RENDER_BENCH=1`, `DEPPY_BENCH_SCENARIO`,
`DEPPY_BENCH_WORKSPACES`, `DEPPY_BENCH_SECS`, `DEPPY_ALLOC_STATS=1`(feature `bench-alloc`).
전부 게이트 뒤 — **미설정 시 평소 실행 경로·성능 불변**. 산출물은 `artifacts/render-bench/`
(원시 JSONL은 git 제외, summary만 추적).

## 7. 측정의 한계

- 단일 머신(M2)·단일 세션. 각 시나리오 1회 실행(런 간 편차 ±20MB RSS 관측).
- 프레임 이벤트는 종료 시 일괄 flush되어 **시간축 분석 불가**(빈도만 산출).
- 수정 후 재측정은 wgpu·ws1·30초 3개 시나리오만(전체 매트릭스 재실행 안 함).
- `dirty1` p95가 수정 후 0.30 → 0.73 ms로 관측됐으나 **단일 런 노이즈로 판단**
  (rows_rebuilt는 1.1 → 0.9로 감소, 재-shaping이 늘어날 구조적 이유가 없다). 재확인 필요.

---

# 부록: 실제 워크로드 실측 + 프로파일링 (2026-07-14 추가)

합성 벤치(`yes` 대량 출력, 정지 idle)가 이 앱의 실제 워크로드를 대표하지 못한다는
문제의식으로 **에이전트 TUI 시나리오**를 추가하고, CPU를 프로파일링했다.

## A. 에이전트 TUI (실제 핫패스 근사)

`agenttui` 시나리오 = alt screen + 스피너(10Hz, 커서 이동으로 한 셀 갱신) + 부분 갱신 +
스트리밍 출력 + 색/박스 드로잉. Claude/Codex TUI의 렌더 구조를 재현한다.

| 지표 | 실측 |
|---|---:|
| 앱 CPU | **2.9 %** |
| frame p95 | **0.50 ms** (예산 16ms의 3%) |
| rows_rebuilt / frame | **0.5** |

**결론: 실제 워크로드는 전혀 부담이 아니다.** 렌더 최적화(PR-05/06)의 근거가 더 약해진다.

## B. 정정 — 수정 전후 CPU는 **변하지 않았다**

앞서 "dirty1 CPU 134% → 26%"로 보고한 수치는 **측정 아티팩트**였다(단일 런, 창 상태/타이밍
불일치). 수정 전 커밋(`6c92cfa`) 바이너리를 따로 빌드해 같은 조건에서 A/B로 재측정한 결과:

| dirty1 (진행률 `\r` 갱신, 15초 구간 누적 CPU) | 라운드1 | 라운드2 | 라운드3 |
|---|---:|---:|---:|
| 수정 전 | 17.03 s | 15.71 s | 18.61 s |
| 수정 후(gen+선택+감지기) | 19.57 s | 17.98 s | 17.04 s |

**차이 없음(노이즈 범위).** 렌더 낭비 수정(`e794c28`)은 rows_rebuilt를 99% 줄였지만
총 CPU는 바꾸지 못했다 — 렌더가 애초에 CPU의 주 소비처가 아니었기 때문이다.

## C. 프로파일링 — 진짜 CPU 소비처

`sample`(macOS)로 dirty1 실행 중(앱 CPU ~124%) 6초 프로파일:

| top-of-stack | 샘플 |
|---|---:|
| `write` (PTY 응답 + 세션 로그 디스크 기록) | 2,321 |
| `read` (PTY 읽기) | 2,294 |
| **`session::status::StatusDetector::on_output`** | **802** |
| alacritty VTE `Handler::input` (파서) | 94 |
| `AlacrittyBackend::feed` | 57 |
| `viewport_snapshot` | 22 |

- **렌더링은 목록에 없다.** 파서조차 감지기의 1/8이다.
- 지배적 비용은 **read/write 시스템콜** — PTY 소비 + 모든 출력 바이트의 redacted 로그 기록.
- `on_output`이 앱 코드 1위인 이유: 진행률/스피너는 `\r`만 쓰고 개행이 없어 line_buf가
  상한(8KB)까지 차는데, 매 청크마다 **버퍼 전체를 재스캔 + drain(memmove)** 했다
  → 스캔 오프셋 도입으로 청크당 O(CAP) → O(청크)로 수정(`354f2d4`).

## D. dirty1이 CPU 100%대인 진짜 이유 — **처리량 바운드**

`while :; do printf '\rprogress=%d'; done`은 **sleep이 없는 무제한 생산자**다. 앱이 빨라지면
더 많은 바이트를 소비할 뿐이라 CPU%는 계속 고정된다. 어떤 최적화도 이 수치를 낮출 수 없고,
낮추려면 **소비 자체를 제한**(입력 rate limit)해야 한다 — 실제 프로그램은 sleep을 넣으므로
이런 부하를 만들지 않는다(agenttui가 2.9%인 이유).

## E. 이 부록이 바꾼 결론

| 항목 | 변경 |
|---|---|
| PR-05/06 판정 | **SKIP 유지·강화** — 실제 워크로드에서 렌더는 CPU 목록에도 안 나온다 |
| 렌더 낭비 수정(e794c28) 가치 | 낭비 제거는 사실(rows_rebuilt −99%)이나 **총 CPU 개선은 0** — 정직히 기록 |
| 다음 최적화 후보 | 렌더가 아니라 **I/O 경로**(세션 로그 기록 버퍼링, PTY read 청크 크기)와 감지기 |

---

# 부록 2: 리페인트 증폭 — 렌더 낭비의 진짜 형태 (2026-07-14 추가)

부록 1은 "렌더는 CPU 목록에 안 나온다"로 끝났다. 그건 **프레임 1장을 그리는 비용**을 본
결론이고, **프레임을 몇 장 그리는가**는 보지 않았다. 후자에 실제 낭비가 있었다.

## A. 증상 — 페인트의 70%가 헛 프레임

`agenttui`에서 프레임 이벤트에 `cause`(egui `repaint_causes()`)를 실어 집계했다.
내용은 초당 9회 바뀌는데(스피너 10Hz) 앱은 **초당 30프레임**을 페인트했다.

| 원인 (wgpu, 45s) | 프레임 | 그중 헛(dirty_rows=0 & rows_rebuilt=0) |
|---|---:|---:|
| `app.rs:4001` (logic()의 이벤트 후 재요청) | 416 | 416 |
| `none` (원인 없음) | 415 | 415 |
| `app.rs:1658` (PTY 뷰포트 wake) | 398 | **12** ← 정상 |
| `workspace.rs:1669` (pane 플래시 페이드) | 96 | 96 |

세 원인이 1/3씩 = **갱신 1회가 정확히 3프레임 체인**을 만든다. 헛 프레임도 58행 전체를
다시 그리고 GPU 패스를 돈다(재shaping만 캐시로 생략될 뿐).

## B. 원인 — 중복 요청 + egui의 settle 프레임

1. **중복 요청**: `logic()`이 이벤트를 드레인한 뒤 `request_repaint()`를 다시 부른다.
   그러나 이벤트를 거기까지 실어나른 **모든 경로**(`emit_gated`의 Viewport/InputPressure/
   ResourceUsage slot + `enqueue_durable_event`)가 이미 wake로 리페인트를 요청했다.
   이번 프레임이 그리고 있는 내용을 위해 프레임을 한 장 더 잡는 셈.

2. **egui의 settle 프레임**: egui는 `delay == 0`인 요청마다 프레임을 **2장** 만든다.
   ```rust
   // egui-0.35 context.rs:137
   if delay == Duration::ZERO {
       // Each request results in two repaints, just to give some things time to settle.
       viewport.repaint.outstanding = 1;
   }
   ```
   두 번째 프레임은 새 cause가 없어 `prev_causes`가 비고 → 계측에 `none`으로 찍힌다.
   **0이 아닌 delay는 이 경로를 타지 않고**, 이어서 `delay -= predicted_dt`로 0이 되어
   결국 즉시 리페인트된다 → `request_repaint_after(1ms)`로 지연 없이 헛 프레임만 뺄 수 있다.

## C. 수정 후 실측 (HEAD vs HEAD+수정, 유일한 차이)

|  | wgpu | glow |
|---|---:|---:|
| 페인트 프레임 | 1330 → **582** (−56%) | 1439 → **686** (−52%) |
| 실렌더(rows_rebuilt>0) | 405 → **410** (+1%) | 411 → **417** (+1%) |
| 헛 프레임 | 925 → **172** (−81%) | 1028 → **269** (−74%) |
| **CPU (정상운행)** | 5.27% → **3.05%** (−42%) | 4.43% → **2.81%** (−37%) |

**실렌더 프레임 수가 보존**되므로 갱신 유실·지연은 없다. `frame p50`이 소폭 오르는 건
회귀가 아니라 구성비 변화다 — 값싼 헛 프레임(≈0.24ms)이 빠지면서 중앙값이 실렌더
프레임(≈0.43ms) 쪽으로 이동한다. 총 작업량(프레임 × ui_ms)은 40%대로 줄고 CPU와 일치한다.

남은 헛 프레임 대부분은 `workspace.rs:1669`의 **2초 pane 플래시 페이드**로, alpha가 매
프레임 바뀌는 의도된 애니메이션이다(터미널 행 기준 지표에서만 헛으로 잡힌다).

## D. 부록 1의 결론 보정

| 항목 | 보정 |
|---|---|
| "렌더는 CPU 목록에 없다" | **프레임당 비용**은 맞다. 그러나 **프레임 수**가 3배 부풀어 있었고, 이를 고치니 실제 워크로드 CPU가 40% 내렸다 |
| PR-05/06(렌더 파이프라인 최적화) | **SKIP 유지** — 이번 수정은 파이프라인이 아니라 리페인트 스케줄링이다 |
| 다음 후보 | 여전히 **I/O 경로**(세션 로그 기록 버퍼링 — 프로파일 1위 `write`)가 최대 건이다 |

## E. `write` 시스템콜 — 실제 워크로드에서는 핫스팟이 **아니다** (기각, 2026-07-14)

부록 1은 "다음 최적화 후보 = I/O 경로(세션 로그 기록 버퍼링)"로 끝났다. 그 근거였던
프로파일은 **dirty1**(sleep 없는 무한 생산자)에서 뜬 것이므로, `agenttui`(실제 워크로드)에서
다시 측정했다. 20초, 정상 운행 구간, 리페인트 수정 적용 후 바이너리, 앱 CPU 3.3%.

| top-of-stack | dirty1 | **agenttui** |
|---|---:|---:|
| `write` | 2,321 | **9** |
| `StatusDetector::on_output` | 802 | **표에 없음** (354f2d4로 해소) |
| `viewport_snapshot` | 22 | 14 |

agenttui 프로파일의 상위권은 전부 **대기**다 — `semaphore_wait_trap`(97,932),
`__workq_kernreturn`(65,107), `__psynch_mutexwait`(48,970), `mach_msg2_trap`(48,321).
`read`(17,712)조차 CPU가 아니라 **PTY 리더 스레드가 블로킹 read로 데이터를 기다리는** 것이다.
즉 실제 워크로드에서 앱은 대부분 놀고 있고, 앱 코드 최대 항목이 `viewport_snapshot` 14샘플이다.

**결론: 로그 기록 버퍼링 최적화는 하지 않는다.** 두 가지 이유다.

1. 실제 워크로드에 write 부하가 없다(9샘플).
2. write가 커지는 고출력 구간은 **처리량 바운드**다(부록 1-D). 거기서 바이트당 비용을 줄이면
   CPU%가 내려가는 게 아니라 초당 더 많은 바이트를 소비할 뿐이다 — 효율이 아니라 처리량이
   바뀐다. 배터리·CPU 관점의 이득은 없다.

부록 1-E의 "다음 후보: I/O 경로" 항목은 이로써 **기각**한다. 현재 실제 워크로드에서
남은 CPU 소비처는 없다(3.3%, 프레임 예산의 3%).
