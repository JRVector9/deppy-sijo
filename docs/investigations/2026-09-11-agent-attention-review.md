# 에이전트 응답 대기 구현 코드 리뷰

기준: `investigate/agent-wait-classification`, main `9432d33`에서 분기.
Codex CLI `review --uncommitted`로 실제 소스 diff를 리뷰했다. 중복 출력 제외 P1 1건,
P2 10건을 모두 반영했다. 이후 자체 검토에서 아래 추가 경계도 수정했다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | storage/agent_attention.rs · 완료 토큰 | 관찰마다 토큰 증가 | 예약 후속 작업 조기 실행 | 새 완료만 증가하도록 수정·재검증 완료 |
| medium | storage/agent_attention.rs · 취소 | 이전 턴 종료가 현재 턴에 적용 | 진행 중 표시 손실 | 현재 턴 비교 추가·재검증 완료 |
| medium | storage/agent_attention.rs · 요청 갱신 | 재사용 별칭의 이전 턴 유지 | 새 요청 취소 누락 | 턴도 함께 갱신·재검증 완료 |
| medium | storage/agent_attention.rs · 완료 | Stop 뒤 작업 재개 시 마지막 완료 누락 | 미확인 결과 손실 | Working에서 완료 중복 판정 초기화·재검증 완료 |
| medium | mcp-proxy/agent_attention.rs · Grok | 유휴를 성공으로 단정 | 취소·실패가 완료로 표시 | 실제 최신 턴 결과 확인·재검증 완료 |
| medium | mcp-proxy/agent_attention.rs · Elicitation | ID 없는 요청 충돌 | 다른 질문까지 해제 | 서버·모드 및 미응답 개수 분리·재검증 완료 |
| medium | app/agent_shim.rs · Codex 질문 | 실패 결과의 PostToolUse 부재 | 끝난 질문이 대기로 고착 | bounded rollout의 call_id 결과 대조·재검증 완료 |
| medium | session/status.rs · Enter 안내 | 제출 후 과거 안내 재감지 | 대기 표시 고착 | 한 줄 continue 안내 소비·재검증 완료 |
| medium | app/ui/agent_sessions.rs · 응답 필요 | 중단 버튼 대상에서 누락 | 질문 중 중단 불가 | NeedsResponse 추가·컴파일 확인 |
| medium | app/app.rs · 슬래시 명령 | 기존 테스트가 구형 상태 통합 전제 | 회귀 검사 실패 | 질문·승인 모두 차단하는 검사로 수정·재검증 완료 |
| medium | storage/agent_attention.rs · 용량 검사 | 중첩 if lint 위반 | strict clippy 실패 | 조건 통합·strict clippy 통과 |

표의 위치는 `crates/<crate>/src/` 기준이다. 모두 수정 완료된 최초 리뷰 지적이며,
현재 남아 있는 결함 목록이 아니다.

## 추가 수정

- ID 없는 동일 서버의 동시 질문은 한 답변으로 전부 해제하지 않는다.
- 늦은 취소가 이미 다시 열린 질문을 지우지 않는다.
- 완료 직후 나타난 TUI 선택창은 완료 표시보다 우선한다.
- 같은 Done 상태 사이에 새 완료가 와도 이전 포커스 이력을 재사용하지 않는다.
- Grok 훅이 불필요하게 Kimi 인덱스를 읽지 않도록 provider 경로를 구분했다.

## 검증과 제한

- storage 355, session 66, mcp-proxy 61 PASS 및 proxy 1 ignored.
- 앱 상태 병합·포커스·표면·슬래시·기존 shim 관련 14 PASS. 화면을 렌더하는 검증은 아니다.
- fmt, workspace all-targets strict clippy, boundary, i18n-check, diff --check가 모두 통과했다. i18n은 리터럴 키 호출 1160건과 5개 locale을 대조했다.
- 회귀 결함은 먼저 RED를 확인한 후 수정했다. 코드 리뷰 원본은 로컬 임시 로그에 보관했다.
- 요청별 저장은 최대 64개/32KiB이며 이벤트 stdin은 256KiB, 근거 로그 읽기는 512KiB/512행 한도다.
- 자유 문장 질문의 의미를 추측하지 않는다. 명시적 질문 신호가 없으면 미확인 완료로 보완한다.
- Grok 완료 근거가 없으면 성공으로 단정하지 않는다. 완료 표시는 정착 알림 시점까지 늦어질 수 있다.
- 실제 CLI 화면 검증과 앱 패키징·재실행은 하지 않았다. 새 훅은 적용 후 새로 시작한 CLI에서 확인한다.

공식 이벤트 계약 재확인: [Claude hooks](https://code.claude.com/docs/en/hooks),
[Grok hooks](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md),
[Codex 0.154.0 질문 도구](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core/src/tools/handlers/request_user_input.rs).
