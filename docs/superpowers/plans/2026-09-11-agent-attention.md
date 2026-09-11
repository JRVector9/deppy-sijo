# 에이전트 응답 대기 구현 계획

**승인 설계:** `docs/investigations/2026-09-11-agent-awaiting-response.md`, 사용자 요청 “제안한대로 구현 하고, 한번더 코드 리뷰하고 보고해”.

**목표:** Claude·Codex·Grok의 질문/승인/미확인 완료를 유휴와 구분하고 다른 프로젝트에서도 유지한다.

**구조:** 훅 페이로드를 의미별 이벤트로 정규화하고 SQLite 트랜잭션에서 요청 ID별 상태를 보존한다. 기존 attention projection의 크기 제한과 workspace namespace를 유지하며 응답 요청 여부를 함께 전달한다. 화면 감지는 훅이 없는 TUI 선택창을 보완한다.

**기술:** Rust, SQLite, 기존 hook helper, egui, 5개 locale.

## 실행 순서

- [x] 저장 계약: `crates/storage/src/agent_attention.rs`에 요청 시작/결과/다른 도구/완료/세션 교체 순서의 실패 테스트를 먼저 추가한다. `cargo test -p storage agent_attention -- --test-threads=1`로 RED를 확인한 뒤 요청 ID 및 이벤트 시각 검증, 크기 상한, 원자적 projection 갱신을 구현한다.
- [x] 훅 어댑터: `crates/mcp-proxy/src/agent_attention.rs`에서 Claude snake_case, Grok camelCase, Codex 질문 도구와 Notification 분기 테스트를 먼저 작성한다. `cargo test -p mcp-proxy agent_attention -- --test-threads=1`로 검증한다. 수신은 관찰 전용이며 `{}` 응답과 오류 시 에이전트 진행을 보존한다.
- [x] 훅 배선: `crates/app/src/agent_shim.rs`에서 Pre/PostToolUse·실패·알림 종류·종료를 정규화 수신기로 보낸다. Grok 전용 훅 파일은 기존 사용자 파일을 덮지 않고 Deppy 파일만 관리한다. 재실행 전 실제 사용자 설정은 변경하지 않는다.
- [x] 화면 보완: `crates/session/src/status.rs`와 `crates/runtime/src/in_process.rs`의 실패 테스트로 방향키/문자 입력이 미응답을 소비하지 않는지 고정한다. 계획 선택창은 알려진 선택 푸터으로 감지하고, 단순 한국어 물음표로 상태를 확정하지 않는다.
- [x] 앱 연결: `crates/app/src/app.rs`, `ui/workspace.rs`, `agent_surface.rs`, fleet 표시와 locale 5개에 응답 필요/승인 필요/완료·확인 필요/다음 지시 대기를 전달한다. 대기는 Working이나 포커스로 지우지 않고, 완료 확인은 앱 포커스 및 실제 선택 상태를 확인한다. 기존 waiting render/빈 세션 계약은 보존한다.
- [x] 재검증과 리뷰: 변경 영역 테스트를 실행하고 Codex CLI에 실제 코드 diff를 리뷰시킨다. 발견을 수정한 뒤 관련 검증을 재실행한다. 커밋 직전 fmt/clippy/boundary/i18n/diff gate를 한 번 실행한다.
- [x] handoff와 프로젝트 일지에 결과를 기록하고 한국어 커밋을 만든다. 앱 빌드·재실행·UI 화면 검증은 이번 범위에서 보류한다.

## 완료 범위

- 코드 리뷰 지적 11건을 반영하고 추가 경계 조건을 수정했다. 상세: `docs/investigations/2026-09-11-agent-attention-review.md`.
- 새 훅 설치와 DB v42 적용은 앱 재실행 이후다. 현재 운영 DB와 사용자 설정은 그대로 유지했다.
- UI 화면 및 세 CLI의 실제 질문/응답 검증은 아직 수행하지 않았다. 앱 빌드·재실행 승인을 받은 후 직접 확인한다.
