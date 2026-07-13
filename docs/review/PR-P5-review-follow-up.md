# PR-P5 설계 리뷰 후속 — 별도 PR 트래킹 (P9 / P10 / P11 / P12)

작성일: 2026-07-13
근거: PR-P5(터미널 뷰어) 설계 리뷰 12건 중, 구현(PR-P5a~d + "P5 리뷰 P1/P2 반영")에
반영되지 않았거나 리뷰 권고와 다른 방식으로 처리된 4건을 별도 PR 후보로 기록한다.
반영 완료 8건(P1~P8)은 코드로 확인됨 — 이 문서의 범위 아님.

---

## PR-F1 — 브리지 dashboard JSON 재구축 게이트 (리뷰 P11) — **구현 완료 (2026-07-13)**

codex(gpt-5.5 high) 사전 리뷰 반영: `Inner.dashboard_dirty` + `apply_event -> bool`
게이트, 즉시 재구축 경로 5곳(register_connection / set_workspaces /
reseed_active_sessions / set_notice(None 포함) / set_runtime_source) 보존,
승인 폴링 회차 재구축 유지, `dash_build_count` 카운터 + 게이트 회귀 테스트 추가.

**우선순위: 1 (리소스/병목 — 남은 항목 중 유일한 실질 성능 개선)**

- 현상: 브리지 run 루프는 접속 ≥1이면 **깨어날 때마다** `ServerMsg::Dashboard{...}.encode()`를
  재구축해 문자열 비교한다 (web-remote/src/dashboard.rs `fn run`, publish 경로).
- 문제: 뷰어 스트리밍 중에는 viewport wake가 초당 수십 회 — 대시보드와 무관한
  wake마다 워크스페이스×세션 규모의 JSON 직렬화+비교가 낭비된다.
  (viewport 자체는 세션별 슬롯으로 이미 분리 반영됨 — staged_viewports → published.viewports)
- 제안: "대시보드 관련 이벤트(MuxUpdated/상태/리소스/워크스페이스 구성)가 이번 drain에
  있었을 때만 재구축" 플래그. 승인 폴링 주기의 재구축은 유지.
- 완료 기준: 스트리밍 중(출력 폭주 세션 1개 시청) 브리지 스레드 CPU 실측 감소,
  대시보드 반영 지연 ≤1s 회귀 없음.

## PR-F2 — 설계 문서 개정 (리뷰 P10) — **반영 완료 (2026-07-13)**

codex(gpt-5.5 high) 사전 리뷰 반영: v2.5 §0.1 원칙 3(render 정의 한정)·4(snapshot
예외 참조)·5(원격 시청 lease 예외 명문화) + §14.4/§14.6 동일 예외, P5 계획서의
범위/구현 요점/리스크를 실구현(baseline diff·RunView·TTL lease·확장 입력)으로 교정.

**우선순위: 2 (성능 무관 — 후속 리뷰 혼선/회귀 방지)**

- 현상 1: v2.5 설계문서(`ai_agent_workspace_final_architecture_v2_5_FINAL.md` §0.1)
  불변 원칙 5 "Terminal snapshot은 visible pane에만 만든다"에 원격 시청 승격
  (SetRemoteViewing lease) 예외가 미기재 — 현 구현이 문면상 원칙 위반 상태.
- 현상 2: P5 계획서(`ai_agent_workspace_v3_3_mobile_pwa_remote_pr_plan.md` PR-P5 §범위)가
  "session.take_dirty_ranges 재사용"이라고 서술 — 실구현은 dirty_ranges를 쓰지 않고
  접속별 baseline diff(ws_api.rs)로 갔다. 문서와 구현 불일치.
- 제안: 원칙 5에 "단, 원격 시청 lease(SetRemoteViewing) 세션은 visible 등가로 승격 —
  스냅샷 생성만 허용, GUI repaint 미유발(emit_gated/render_bound)" 예외 명문화.
  P5 계획서의 프레임 소스 서술을 실구현(접속별 baseline diff + keyframe 폴백,
  행 텍스트+스타일 run 인코딩)으로 교정.

## PR-F3 — (모니터링) 시청 승격의 hidden 스크롤백 캡 해제 (리뷰 P9, 대안 채택)

**우선순위: 3 (메모리 — 현재는 리스크 처리됨, 실측 후 판단)**

- 리뷰 권고는 "승격은 스냅샷 게이트만, §14.3 캡 불변"이었으나, 구현은 승격 시
  hidden cap 해제 + 해제/만료 시 reconcile_visibility 즉시 재적용을 채택
  (in_process.rs SetRemoteViewing 핸들러). TTL lease(5분 캡, 갱신형)가 브리지
  사망 시 자동 원복 백스톱 — 원복 리스크는 해소된 것으로 판단.
- 남는 관찰 항목: 장시간 시청(lease 계속 갱신) 중 고출력 세션의 스크롤백이
  uncap 상태로 자라는 메모리 상승. §14.3 전역 캐시 예산 안이라 유계지만,
  폰 시청 실사용에서 RAM 실측 후 "시청 중에도 hidden cap 유지" 재검토 여지.
- 액션: 별도 PR 불필요(현행 유지). 실측에서 시청 중 RAM 증가가 +10MB 예산을
  넘으면 이 문서를 근거로 PR화.

## PR-F4 — (모니터링) 아카이브 세션 시청 inflate (리뷰 P12, 대안 채택)

**우선순위: 4 (메모리 — 의도적 기능 확장, 관찰만)**

- 리뷰 권고는 "v1은 라이브 세션만"이었으나, 구현은 아카이브 세션도 inflate해
  시청 허용(push_watched_viewports의 원격 대상 inflate_archived). stale id는
  거부해 유령 lease 차단(SetRemoteViewing `known` 검사).
- 남는 관찰 항목: 폰에서 오래된 아카이브 세션을 여러 개 순회 시청하면 inflate로
  압축 backend가 연속 복원 — exited-retained 예산(archive_over_cap) 회전 부하.
- 액션: 별도 PR 불필요(현행 유지). 아카이브 다수 순회 시청 시나리오에서 메모리/CPU
  스파이크가 관측되면 "아카이브 시청은 명시 확인 후 inflate" UX로 PR화.

---

## 처리 순서 제안

1. PR-F1 (코드 — 작음, 즉시 가능)
2. PR-F2 (문서 — 작음, 즉시 가능)
3. PR-F3/F4 (실측 게이트 — v3.3 P5 완료 기준의 리소스 실측과 함께 판정)
