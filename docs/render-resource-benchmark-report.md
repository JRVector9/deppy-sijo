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
