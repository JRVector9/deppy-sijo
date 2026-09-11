# fc9bd03 에이전트 대기 상태 재리뷰

요청: 전부 수정했는지, 더 수정할 것이 없는지 검토.
기준: `investigate/agent-wait-classification`, 커밋 `fc9bd03`.
당시 검토 결론: 최초 지적 반영 후 아래 7건이 남았다. 이후 사용자 `수정해` 요청으로 전부 수정했으며, 수정·검증 내역은 문서 아래에 기록했다. 아래 위치는 검토 당시 fc9bd03 기준이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/mcp-proxy/src/main.rs:330 | Claude 계획 거절 결과 미반영 | 거절 후에도 응답 대기로 고착 | Claude tool_result도 요청 ID별 대조 |
| medium | crates/mcp-proxy/src/agent_attention.rs:26 | 서브에이전트 승인 훅을 통째로 무시 | 승인 감지가 화면·추가 알림에 의존 | 자식 종료와 사용자 요청 이벤트 분리 |
| medium | crates/storage/src/agent_attention.rs:160 | 취소보다 늦게 저장된 과거 질문 부활 | 취소 후 다시 대기 표시 | 턴별 종료 경계 저장 및 이전 요청 거절 |
| medium | crates/storage/src/agent_attention.rs:141 | 익명 동시 질문의 저장 순서 역전 미처리 | 미응답 질문이 목록에서 사라짐 | 시작·결과를 순서와 무관하게 합산·대조 |
| medium | crates/storage/src/agent_attention.rs:202 | Stop 이후 질문 재개 시 완료 중복 판정 미갱신 | 최종 완료 표시 누락 | 질문·승인 재개도 완료 세대에 반영 |
| medium | crates/session/src/status.rs:359 | 일반 한 줄 질문의 제출을 소비하지 않음 | 답변 뒤에도 응답·승인 대기 고착 | 실제 제출과 선택 이동 구분 확대 |
| medium | crates/app/src/app.rs:16971 | 실제 터미널 가시성을 읽음 판정에 반영하지 않음 | 보지 않은 완료를 읽음 처리 | 중앙 화면·보조 탭 가시성까지 확인 |

## 재현 근거

1. **Claude 계획 거절:** PreToolUse(ExitPlanMode, plan-1) → PermissionRequest → 수동 거절 → Stop → 다음 UserPromptSubmit → Stop에도 `pending=[plan-1]`이다. 수동 permission denial은 PostToolUseFailure/PermissionDenied로 돌아오지 않는다는 [공식 훅 계약](https://code.claude.com/docs/en/hooks#posttoolusefailure)을 대조했다. `PermissionDenied`만 등록하는 것으로는 수동 거절을 해결할 수 없다. 재현 파일 `/tmp/fc9bd03-review-repro.rs`, 실행 파일 `/tmp/fc9bd03-review-repro`.
2. **서브에이전트 승인:** 같은 PermissionRequest 페이로드에 agent_id를 추가하면 normalize가 ApprovalRequired 대신 None을 반환한다. [공식 subagent 계약](https://code.claude.com/docs/en/sub-agents#run-subagents-in-foreground-or-background)에서는 전경·배경 서브에이전트 모두 사용자 승인 요청을 메인 대화로 전달한다. [공식 훅 필드](https://code.claude.com/docs/en/hooks#common-input-fields)는 자식 훅에 agent_id가 있음을 명시한다. 실행 파일 `/tmp/deppy-attention-subagent-repro`. 추가 Notification이나 화면 감지에 의한 보완 가능성과 별개로 이 직접 신호는 현재 버려진다.
3. **늦은 질문 부활:** TurnStart@1 → Cancelled@3 → ResponseRequired(q)@2의 DB 저장 순서에서 waiting=1. hook subprocess의 커밋 순서는 이벤트 시각 순서를 보장하지 않는다.
4. **익명 질문 누락:** 같은 서버 ResponseRequired@1 → Resolved@3 → ResponseRequired@2 저장 순서에서 waiting=0. 실제로 두 질문 중 한 질문에만 답했지만 두 번째 시작을 낡은 갱신으로 버린다.
5. **질문 재개 후 완료 누락:** TurnStart → Completed → ResponseRequired → Resolved → Completed에서 done=0. 앞선 수정은 Working 재개만 처리했고 질문·승인 재개는 completed_turn을 초기화하지 않는다. 3~5 실행 파일 `/tmp/deppy-attention-second-repros`, 소스는 같은 경로의 `.rs`.
6. **한 줄 입력 고착:** Enter your name, Paste your token, Type yes to continue에 답을 보내고 프로그램이 셸로 돌아와도 Waiting이 남는다. `Continue? [y/n]`은 echo가 stream regex를 다시 걸어 과거 문구가 화면에서 사라져도 NeedsApproval을 유지한다. parent와 비교한 독립 재현도 parent=Running, commit=NeedsApproval이다. `/tmp/deppy-attention-line-prompt-repro`, `/tmp/fc9bd03-screen-compare`.
7. **가시성 누락:** `session_entries`의 focused는 mux.focused_pane만 확인한다. `update_session_alerts`는 앱 포커스만 추가해 완료 clear를 예약한다. 이 호출은 중앙 화면 분기보다 앞이며, Home/Fleet에서는 터미널을 렌더하지 않는 분기가 app.rs:30398, 30541에 있다. 실제 UI를 조작한 검증은 하지 않았다. 기존 읽음 처리에서도 남아 있는 누락으로, fc9bd03에서 새로 생긴 회귀라고 단정하지 않는다.

## 검증 상태

- Codex CLI `review --commit fc9bd03` 완료. 외부 리뷰 지적 3건 중 취소 순서·입력 고착 2건은 자체 검토와 중복이며, 수동 계획 거절 1건을 추가했다.
- 기존 관련 테스트 재실행은 storage 355 + session 66 + proxy 61 = 482 PASS, 1 ignored.
- 외부 리뷰에서 strict workspace Clippy, fmt, diff check도 exit 0을 확인했다.
- 자체 진단 3개와 외부 진단은 현재 소스/라이브러리를 사용해 **결함을 재현**했다. 수정 후 PASS를 의미하지 않는다.
- ElicitationResult만 보내고 실제 elicitation_response 알림을 생략한 초기 외부 재현은 정상 provider 흐름의 결함 근거에서 제외했다.
- 운영 DB·사용자 훅 설정·앱 실행·release bundle은 변경하지 않았다. 소스 변경도 없다. 문서와 임시 진단 파일만 작성했다.

## 권장 수정 순서

1. 요청 누락/고착의 공통 수명 모델: Claude 거절 결과, 자식 요청 식별, 턴 취소 경계, 익명 요청의 순서 역전.
2. 질문 재개 후 완료 세대와 실제 화면 가시성에 따른 읽음 처리.
3. 터미널 입력 종류별 제출 처리와 위 재현의 회귀 테스트 추가.
4. 해당 경계 테스트·정적 검사 후 승인된 새 앱으로 실제 질문·거절·프로젝트 이동 화면 확인.


## 후속 수정 결과

- 7건 모두 수정했다. 기존 미응답 요청의 ID와 부모/자식 소유자를 보존하며 요청 수명과 완료 알림을 분리했다.
- Claude 수동 거절: 현재 [PostToolBatch](https://code.claude.com/docs/en/hooks#posttoolbatch)의 요청 ID/도구 입력과 제한된 transcript의 `tool_result` ID를 대조한다. 과거 transcript에서 fingerprint만 같다는 이유로 새 승인을 해제하지 않는다. 자식 결과도 같은 자식의 요청만 닫는다.
- 서브에이전트: 자식 질문·승인을 추적한다. 자식 도구 목록은 결과 해제 근거로만 쓰며 부모의 작업/완료 상태는 바꾸지 않는다. 같은 소유자의 도구·승인이 전부 해제돼야 ID 없는 permission 알림을 닫는다. SubagentStop을 부모 완료로 취급하지 않는다.
- 순서 역전: 취소 경계를 저장해 취소된 턴의 늦은 질문을 차단한다. 익명 질문 시작/응답은 잔여 개수와 중복 이벤트 기록으로 대조한다. 기록은 세션 전체 256개로 제한하고 개수는 유지한다. 익명 요청을 새 턴에서 재사용할 때 소유 턴도 갱신한다.
- 완료: 질문·승인으로 재개된 턴도 새 완료를 남긴다. 홈·작업 화면, 보조 탭·설정, 비활성 창, 다른 workspace pane을 보고 있을 때 primary 완료 알림을 소비하지 않는다. 실제 활성 mux 탭도 확인한다.
- 입력: 한 줄 질문의 실제 Enter 제출을 소비한다. 선택 이동·작성 중인 문자·여러 줄 bracketed paste는 제출로 소비하지 않는다. 붙여넣기 표식이 여러 입력 청크로 나뉘어도 처리한다.

### 수정판 독립 코드 리뷰

실제 Codex CLI가 변경 Rust 소스 6개를 리뷰했다. 자체 검토와 겹친 자식 알림 및 익명 턴 재사용, 독립 리뷰에서 재현한 붙여넣기 문제를 반영했다. 최종 지적은 거절 배치의 일반 도구 ID가 결과 후보에서 빠지는 P2 1건이었다. 이 지적도 실행 도구 ID를 포함하는 결과 조회로 수정했고, 동일 자식 수동 거절 통합 회귀는 RED 후 PASS했다. 리뷰 로그는 `/tmp/deppy-attention-fix7-review.log`다.

최종 관련 테스트 **501 PASS, 1 ignored**, 외부 재현 **4 PASS**이며 fmt/strict workspace clippy/check-boundary/i18n-check/diff 검사 모두 통과했다. 정확한 명령과 결과는 `docs/CODEX_HANDOFF.md`의 이번 수정 완료 절에 기록한다. 실제 Claude/Codex/Grok 대화 화면은 검증하지 않았으며 앱 패키징·재실행·main 머지도 하지 않았다. 운영 DB 및 사용자 훅 설정은 변경하지 않았다.
