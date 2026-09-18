# 2026-09-18 세 CLI 실행 중 / 지시 대기 재검토

- 요청: 폴더 트리 수정을 마친 뒤 Claude·Codex·Grok 세션의 실행 중/지시 대기 표시를 확인한다.
- 기준: `fix/environment-api-context-integration`, HEAD cc2362d와 현재 미커밋 변경. 실행 앱은 92859ff 입력 bundle이며 상태 판정 소스는 이번 작업에서 변경하지 않았다.
- 범위: 표시 병합, 구조화 로그 파서, 훅 설정/정규화, 저장소 투영, 실제 설치 메타데이터 확인. 사용자 세션에 새 입력을 보내거나 앱을 재시작하지 않았다. 대화 본문·도구 인자·비밀 값은 출력하지 않았다.

## 기본 연결

| CLI | 실행/완료 근거 | 질문/승인 근거 |
| --- | --- | --- |
| Claude | UserPromptSubmit/PreToolUse, Stop; transcript의 user/tool_use/end_turn | AskUserQuestion/ExitPlanMode, PermissionRequest, Elicitation |
| Codex | UserPromptSubmit/PreToolUse, Stop; task_started/task_complete/turn_aborted | request_user_input, PermissionRequest |
| Grok | UserPromptSubmit/PreToolUse, task_complete/idle_prompt; events.jsonl의 turn_started/turn_ended | ask_user_question, permission_prompt |

`agent_shim.rs`의 설치 결과를 읽어 Claude per-invocation JSON, Codex hooks 활성화/이벤트 인수, Grok `deppy-status.json`의 실제 존재와 이벤트 목록을 확인했다. 전역 Claude/Codex 설정에서 Deppy 훅이 없다는 사실만으로 미설치라고 판정하면 안 된다. 이 앱은 실행 래퍼로 주입한다.

`workspace.rs::merge_agent_status`는 미응답 요청 → 화면 오류/질문/승인 → 완료 → 작업 중 훅 → transcript → PTY 추정 순서로 병합한다. `status.idle`는 지시 대기, Waiting은 응답 필요, NeedsApproval은 승인 필요로 분리돼 있다. 완료 후 실제 터미널 확인이 이루어진 세대만 소비하는 처리도 유지된다.

## 확인한 결함: 비활성 워크스페이스의 긴 작업 오분류

- `app.rs::poll_agent_detect`는 active 세션만 transcript 감지 워커에 보낸다.
- 사이드바의 warm runtime은 `session_entries`에 빈 `no_activity`를 넘긴다. 따라서 다른 워크스페이스의 세션에는 transcript의 Working 근거가 전달되지 않는다.
- `storage/src/db.rs`의 `WORKING_SESSIONS_SELECT`는 완료 여부와 무관하게 `updated_at > now - 120`인 working 행만 반환한다. 오래 생각하거나 출력 없는 긴 도구가 실행되면 다음 PreToolUse가 오기 전 만료될 수 있다.
- `session/src/status.rs`는 명시적 화면 프롬프트 없이 출력이 10초 멈추면 IdleHeuristic을 생성한다.
- 그 결과 작업 중 훅이 만료되고 transcript 보완이 없는 warm 세션은 실제 완료 전에도 지시 대기로 표시될 수 있다. 세 CLI 모두 공통 경로의 영향을 받는다. 현재 스크린샷의 특정 행이 이 상황이었다고 단정하지는 않는다.

### 재현

실제 `merge_agent_status` 본문을 추출한 독립 진단과 실제 SQL을 메모리 DB에서 실행했다. 도메인 enum만 진단용으로 선언했고 병합 함수 본문과 SQL은 현재 소스를 사용했다.

- active + transcript Working + PTY Idle → Running.
- warm + transcript 없음 + 작업 중 훅 만료 + PTY Idle → Idle.
- 작업 중 훅이 살아 있으면 Running, 명시적 미응답 질문은 Waiting.
- DB의 working=1/완료 없음 상태를 유지해도 119초에는 조회되고 121초에는 조회 결과에서 제외됨.

산출물: `/tmp/deppy-agent-status-proof-20260918/{merge-proof.rs,merge-proof,compile.log,result.log}`. 진단 컴파일/실행 exit0. 이는 실제 CLI를 2분간 조작한 통합 재현이 아니라 현재 소스/SQL의 조건 재현이다.

## 검증과 적용 상태

기존 집중 검사 **8 PASS / 0 FAIL / 2305 filtered**, `/tmp/deppy-agent-status-audit-20260918.log`:

```sh
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo test --locked -p deppy-sijo --bin deppy-sijo --features bench-alloc -- --nocapture claude_idle_when_end_turn claude_working_when_tool_use codex_idle_and_working_and_session_id parse_grok은_summary와_마지막_레코드로_상태를_만든다 grok은_중간_assistant를_turn_end_전까지_working으로_유지한다 merge_agent_status_hook_working_우선순위 merge_agent_status_완료_후_선택창은_응답을_요청한다 응답대기_최신_대기는_작업중_기록으로_지우지_않는다
```

실제 DB는 읽기 전용으로 훅 상태 플래그/시각만 확인했다. 확인 순간의 최근 Claude 훅은 Working이었고, 확인 가능한 Codex/Grok 기록에는 완료 이벤트가 있었다. 이는 과거 스크린샷의 모든 세션과 일대일 대조한 결과가 아니다. 질문/승인/완료의 신규 attention_json 행은 24시간 만료 예외가 있으므로 그 상태까지 일괄 시간 만료된다고 주장하지 않는다.

이번 요청은 상태 확인으로, 위 분류 결함의 제품 수정은 아직 하지 않았다. 권장 후속은 워크스페이스·runtime·session으로 격리된 최신 활동 근거를 비활성 세션에도 유계로 제공하고, 시간 만료만으로 지시 대기를 확정하지 않는 것이다. 단순히 120초를 늘리거나 Working을 무기한 고정하면 완료 훅 유실 시 반대 오분류가 생길 수 있다. 새 타이머/전체 로그 반복 읽기는 피해야 한다.

폴더 트리 재리뷰의 두 보완은 별도로 완료했고 관련 11건이 통과했다. 전체 제품 변경은 미커밋이며 앱 재빌드·재실행·화면 검증은 아직 하지 않았다.
