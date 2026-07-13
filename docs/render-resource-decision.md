# 렌더러 개선 최종 판정 (PR-04 ~ PR-06 게이트)

작성일: 2026-07-14
근거: `docs/render-resource-benchmark-report.md`(실측), `docs/render-path-analysis.md`(정적 분석),
`docs/terminal-current-state-audit.md`(코드 감사)

---

## 최종 판정: **WGPU_BACKEND_ONLY**

> wgpu/Metal 경로를 유지하고, **PR-05(custom wgpu terminal renderer)와 PR-06(glyph
> renderer)은 진행하지 않는다.** 현재의 epaint + dirty-row galley cache를 유지한다.

### 판정 조건 충족 여부

| WGPU_BACKEND_ONLY 조건 | 실측 결과 | 충족 |
|---|---|---|
| Wgpu가 안정적 | 6개 시나리오 × ws{1,5,10,20} 전부 정상 종료, adapter=Apple M2/Metal | ✅ |
| 메모리 증가가 허용 범위 | glow 대비 ±20MB(런 편차 내), workspace당 +10MB로 선형 | ✅ |
| CPU/프레임 성능이 같거나 개선 | 전 시나리오 p95 ≤0.88ms(예산 16ms의 5%), dirty1은 wgpu가 41% 빠름 | ✅ |
| egui shape/tessellation이 병목이 아님 | 200×60 일반 화면 0.33ms = **예산의 2.0%** | ✅ |
| 화면 품질이 충분함 | 두 백엔드가 동일 epaint 텍스트 경로 — 이론상 동일. **육안 비교 미실시(UNKNOWN)** | ⚠️ |

**⚠️ 1건은 UNKNOWN이나 판정을 바꾸지 않는다**: glow와 wgpu는 같은 epaint 래스터 결과를
업로드할 뿐이고, 현재 기본 경로가 이미 wgpu이므로 "품질이 나쁘면 지금 이미 나쁜 것"이다.
사용자 보고된 품질 문제는 없다. (캡처 비교는 필요 시 `render-path-analysis.md` 절차로 수행)

---

## PR별 결론

### PR-04 (wgpu/Metal) — **이미 완료. "전환"이 아니라 "검증"이었다**

eframe 0.35의 default feature가 wgpu라 **앱은 처음부터 Metal 위에서 돌고 있었다**.
이번 작업으로 확인·문서화했고, 부수적으로 `DEPPY_RENDERER=glow`로 되돌릴 수 있는 **런타임
롤백 경로**를 확보했다(기본값은 wgpu 유지 — 회귀 없음).

- 남은 작업: 계획 문서의 PR-04 서술을 "전환 → 검증 완료"로 정정. Windows D3D12 / Linux
  Vulkan은 wgpu가 자동 선택하므로 플랫폼 종속 코드 없음(이미 충족).

### PR-05 (custom wgpu terminal renderer) — **진행하지 않는다 (SKIP)**

계획서 §9의 진행 조건 6개 중 **참인 것은 2개지만, 둘 다 절대 비용이 예산의 2%라 REPLACE
기준 미달**이다:

| 조건 | 판정 | 근거 |
|---|---|---|
| visible row shape 발행 비용이 큼 | ❌ | 200×60 = 0.33ms (예산 2.0%) |
| tessellation이 frame p95의 주요 비중 | ⚠️ 참이나 무의미 | egui 0.35는 테셀레이션을 캐시하지 않는다(소스 확인) — 매 프레임 재실행하지만 **총 p95가 0.9ms** |
| 대량 출력 CPU가 목표 초과 | ❌ | bulk p95 0.66~0.88ms |
| workspace 수 증가 시 visible shape 비용 과도 | ❌ | 비활성은 렌더 안 함 — ws20에서도 p95 동일 |
| steady-state frame allocation이 egui에서 제거 불가 | ⚠️ 참 | 2,823 alloc/frame은 대부분 epaint 내부. **하지만 성능 영향이 관측되지 않는다** |
| selection/cursor만 바꿔도 전체 shape 재발행 | ✅ 참 | 하지만 그 비용이 0.33ms(선택 run 병합 후) |
| wgpu 전환만으로 목표 미달 | ❌ | 모든 목표 충족 |

**결정적 근거**: PR-05로 얻을 최대 이득(shape 발행 + 테셀레이션 완전 제거)이 **약 0.3ms/프레임**인데,
그 대가로 폰트 폴백·CJK·emoji·2:1 폭 정합·IME·selection 렌더를 전부 직접 떠안는다(현재 egui가
무료로 처리). **투자 대비 이득이 명백히 음수다.**

대신 **실측이 찾은 낭비 2건을 15~40줄로 고쳐 더 큰 이득을 얻었다**(아래 §완료된 개선).

### PR-06 (glyphon/custom glyph renderer) — **진행하지 않는다 (SKIP)**

- PR-05에 종속(custom renderer 없이는 독립 atlas가 의미 없음) → PR-05 SKIP이므로 자동 SKIP.
- 품질 조건: D2Coding 기본(한글 2:1 폭 정합) + wide/spacer 처리 + CJK/emoji 폴백 + 글리프 치환이
  이미 구현. 품질 불만 보고 없음.
- **단, 별개의 기능 격차 1건을 확인했다**(PR-06과 무관):
  `TerminalCell`에 bold/italic/underline/strikeout 필드가 없고, alacritty backend가 해당
  flag를 **읽지도 않고 버린다**. 이는 glyph 렌더러 문제가 아니라 **스냅샷 스키마 + 백엔드
  격차**다 → 아래 백로그 B-1.

### shared buffer pool / pipeline cache — **N/A**

현재 워크스페이스는 GPU 객체를 **하나도 소유하지 않는다**(갤리 캐시만). 도입할 대상 자체가
없다. PR-05에 종속 → SKIP.

---

## 이번 작업으로 완료된 개선 (커밋 `e794c28`)

실측이 아니었다면 발견하지 못했을 낭비 2건 — **코드 감사와 정적 분석은 둘 다 KEEP 판정**했다.

1. **stale dirty_ranges 재-shaping** — 같은 스냅샷을 다시 그리는 repaint마다 전 행 재빌드.
   스냅샷 세대 도입으로 rows_rebuilt/frame: idle 57.6→**0.7**(−99%), bulk 57.5→**7.4**(−87%),
   bulk p95 0.88→**0.66ms**.
2. **선택 하이라이트 셀별 rect** — 연속 셀 run 병합. 200×60 전체 선택에서 2.23ms → 예상 0.3ms대.

교훈: **"캐시가 있다"는 코드 사실이지 성능 사실이 아니다.** 두 결함 모두 단위 테스트를
통과했고 정적 분석도 통과했다 — 실제 앱을 계측해야만 드러났다.

---

## 백로그 (측정으로 판정할 PR만 남긴다)

| ID | 항목 | 근거 | 우선순위 |
|---|---|---|---|
| **B-1** | 텍스트 속성(bold/italic/underline/strikeout) 지원 | `TerminalCell`에 필드 없음 + alacritty backend가 flag 폐기. **기능 격차**(성능 아님) | 중 — 사용자 요구 시 |
| **B-2** | steady-state frame allocation(2,823/프레임) 축소 | §3-B "zero allocation" 목표 미달. 성능 영향은 미관측 | 낮음 — 목표 재검토 필요 |
| **B-3** | Retina/멀티모니터 육안 품질 캡처 비교 | 이번 UNKNOWN 1건. 절차는 `render-path-analysis.md` | 낮음 — 문제 보고 시 |
| **B-4** | selection drag frame p95 실측 | 자동화 불가(수동 절차 존재). run 병합 수정 효과 확인용 | 낮음 |
| **B-5** | 워크스페이스 warm 상한(8) 때문에 ws10·ws20 실효 9개 | 스케일 실측의 한계 — 상한 위 구간 미검증 | 낮음 |
| ~~PR-05~~ | ~~custom wgpu terminal renderer~~ | **SKIP 확정** — 이득 0.3ms vs 폰트/IME/CJK 자체 구현 부담 | — |
| ~~PR-06~~ | ~~glyphon/custom glyph~~ | **SKIP 확정** — PR-05 종속 + 품질 목표 충족 | — |
| ~~shared buffer pool~~ | ~~GPU buffer pool/pipeline cache~~ | **N/A** — workspace가 GPU 객체를 소유하지 않음 | — |

계획서(`terminal-renderer-audit-first-pr-plan.md`)의 나머지 PR 판정은
`terminal-current-state-audit.md`를 따른다(PR-02/03/07 축소, PR-08 부분 진행, PR-09 IMPROVE 5건).

---

## 롤백 방법

| 대상 | 방법 |
|---|---|
| 계측 인프라 전체 | 태그 `pre-render-bench`(`ff26dfe`)로 `git reset --hard`. 로컬 번들 스냅샷: `artifacts/pre-render-bench-app.app` |
| 렌더러만 되돌리기 | **코드 변경 불필요** — `DEPPY_RENDERER=glow`로 실행(런타임 스위치) |
| 계측 오버헤드 | 기본 실행은 전부 게이트 뒤라 오버헤드 0. `bench-alloc` feature는 기본 빌드에 코드 자체가 없음 |
| 렌더 낭비 수정(`e794c28`)만 되돌리기 | 해당 커밋 revert — 단, 회귀 테스트 2건이 함께 빠진다 |
