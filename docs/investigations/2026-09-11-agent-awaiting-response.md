# 에이전트 질문·선택 대기가 유휴로 보이는 문제 조사

- 요청: Grok, Claude, Codex에서 사용자 입력·선택을 기다리는 상태를 유휴와 구분할 수 있는지 확인.
- 조사 기준: main `9432d33` (`763f8b6` 이후 문서 변경만 있음).
- 작업 위치: `/Users/jr/Desktop/projects/deppy-sijo-agent-wait-audit`, 브랜치 `investigate/agent-wait-classification`.
- 설치 버전: Claude Code 2.1.268, Codex CLI 0.154.0, 공식 Grok npm 패키지 1.0.25.
- 이번 작업은 조사와 합성 입력 재현이다. 앱 코드·사용자 CLI 설정·운영 DB를 변경하지 않았다. 앱 빌드·재실행·실제 질문 창 화면 검증은 수행하지 않았다.

## 확인된 원인

### 1. 일반 문장 질문과 작업 완료가 같은 상태가 된다

`crates/app/src/agent_transcript.rs`의 `AgentActivity`는 `Working`, `Idle`만 갖는다.

- Claude `parse_claude`(757행): `stop_reason=end_turn`이면 내용이 질문인지와 무관하게 Idle. `AskUserQuestion`의 `tool_use`는 Working.
- Codex `parse_codex`(1899행): `task_complete`/`turn_aborted`이면 Idle. 질문 도구 호출/결과 및 사용자 질문 이벤트를 대기로 분류하는 경로가 없다.
- Grok `grok_event_activity`(1066행): `turn_started`/`interjected`는 Working, `turn_ended`는 Idle. 질문 사유와 미응답 여부를 보존하지 않는다.

`어느 방법을 선택할까요?`로 끝나는 합성 완료 레코드는 세 파서 모두 Idle로 읽었다. 이는 질문의 의미를 자동으로 정확히 판독할 수 있다는 뜻이 아니다. 자유 문장 질문에는 명시적 도구 신호가 없으므로, 모든 새 최종 답변에 별도 미확인 상태를 보존하는 방법이 필요하다.

### 2. Claude 질문 도구와 대기 훅이 충돌한다

`crates/app/src/agent_shim.rs:94`의 Claude 오버레이는 모든 `PreToolUse`를 clear, 모든 `Notification`을 needs-input으로 보낸다. 질문 도구도 일단 작업 중으로 기록하고, 알림의 `idle_prompt`, `permission_prompt`, elicitation 시작·응답 등을 구분하지 않는다.

`crates/mcp-proxy/src/main.rs:309`는 알림 메시지만 읽어 bool waiting에 기록한다. 구조화된 대기 사유를 보존하지 않는다.

`crates/app/src/ui/workspace.rs:8822`는 needs_input=true여도 transcript가 Working이면 대기 신호를 무시한다. 실제 main 함수와 파서로 만든 합성 조건 `AskUserQuestion 미응답 + needs_input=true + regex Idle`은 Running이 됐다. 오래된 신호를 지우려는 조건이지만 이벤트 순서나 질문 ID 없이 Working만 비교하므로 새 질문도 가려질 수 있다.

설치된 Deppy Claude 오버레이에서도 같은 이벤트 집합과 Notification matcher 없음이 확인됐다. helper와 DB 경로는 존재했다. 단순 훅 파일 미설치만으로 설명되는 문제가 아니다.

### 3. Codex 질문·선택 전용 연결이 없다

`crates/app/src/agent_shim.rs:124`는 PermissionRequest → needs-input, 모든 PreToolUse → clear를 연결한다. 승인 요청과 `request_user_input`을 구분하는 처리가 없다.

설치 버전과 같은 공개 태그 `rust-v0.154.0`의 `core/src/tools/handlers/request_user_input.rs` 및 `core/src/tools/registry.rs`를 확인했다. 질문 도구는 별도의 사용자 응답을 기다리고 일반 함수 도구의 PreToolUse 경로를 이용할 수 있으므로, 도구 이름/호출 ID에 따른 관찰 처리를 설계할 근거가 있다. 승인 훅만 질문 전체를 포괄한다고 가정하면 안 된다.

계획 적용 여부 같은 TUI 자체 선택창은 일반 질문 도구와 별도 검증해야 한다. 상류 이슈 #19328에도 이 격차가 보고돼 있지만, 이슈만으로 설치 버전의 모든 화면 동작을 검증했다고 주장하지 않는다.

### 4. Grok은 Deppy 상태 훅이 연결돼 있지 않다

`agent_shim::install`은 Claude/Codex와 Kimi만 설치한다. Grok은 transcript/events 기반이다. 로컬 `~/.grok/hooks`에 다른 앱의 훅 두 개는 있었지만 Deppy 훅 항목은 없었다. DB의 훅 바인딩과 attention 행 조인에서도 Grok 행은 없었다. DB 조회는 읽기 전용이고 대화·비밀 값은 출력하지 않았다.

공식 로컬 문서 `~/.grok/docs/user-guide/10-hooks.md`는 PreToolUse/PostToolUse/Notification과 `idle_prompt`, `permission_prompt`를 지원한다고 명시한다. 설치 바이너리에도 해당 이름과 `ask_user_question`이 있다. 실제 질문 도구를 실행해 훅 발화 순서를 검증하는 것은 남아 있다.

`crates/app/src/app.rs:27419`, `29531`에서 다른 워크스페이스의 세션에는 transcript activity를 비워 전달한다. 따라서 Grok처럼 Deppy 훅이 없는 세션은 프로젝트를 떠나면 화면 감지와 출력 정지 추정에 더 의존한다.

### 5. 화면 감지와 입력 소비 조건이 실제 미응답 상태를 놓친다

- `crates/session/src/status.rs:158`: 화면 마지막 비어 있지 않은 5줄만 검사한다. 선택창이 위에 있고 푸터가 길면 놓친다.
- `status.rs:217`: 내장 패턴은 제한된 영어 문구다. 합성 한국어 질문/선택지와 `Enter to select · Tab to navigate · Esc to cancel`은 대기로 잡히지 않았다.
- `crates/runtime/src/in_process.rs:2113`: 수락된 모든 PTY 입력에 `detector.on_input()`을 호출한다. 방향키나 답변 작성 시작도 제출과 동일하게 처리한다.
- `status.rs:346`: 현재 프롬프트를 소비한 것으로 표시해 같은 화면의 재감지를 억제한다.
- `status.rs:515`: 새 출력 없이 10초가 지나면 Running → Idle.

합성 재현에서 승인 문구 감지 → on_input → 동일 문구 유지 → 11초 출력 정지 순서로 Idle이 됐다. 실제 방향키를 입력한 CLI 화면을 재현한 것은 아니며, 입력 종류를 검사하지 않는 호출부와 분류 함수를 함께 확인한 결과다.

## 권장 수정 경계

1. 활동 상태와 사용자 확인 사유를 분리한다. Working 중에도 비동기 질문이 남을 수 있으므로 단일 실행/대기 enum만으로 모든 상태를 표현하지 않는다. 표시 용어는 `작업 중`, `응답 필요`, `승인 필요`, `완료·확인 필요`, `다음 지시 대기`로 구분한다.
2. 질문/승인은 workspace·session·턴·요청 ID에 묶고 시작·응답·취소/실패·세션 종료 이벤트로 해제한다. 단순 시간 경과, 방향키, 포커스 변경, 다른 도구 실행, 오래된 Working 레코드만으로 미응답 질문을 지우지 않는다. 신호 세대/시각을 비교해 이전 요청의 늦은 이벤트를 배제한다.
3. Claude AskUserQuestion/ExitPlanMode와 Notification 종류, Codex request_user_input 및 계획 선택창, Grok ask_user_question와 Notification을 각 어댑터에서 같은 의미로 정규화한다. 훅은 관찰만 하며 자동 답변·승인으로 동작을 바꾸지 않는다. 질문 완료/취소 및 Stop이 실제 완료인지도 구분한다.
4. 다른 프로젝트로 이동해도 같은 대기 사유를 사이드바·작업 목록·알림에 전달한다. transcript를 보완할 경우 전체 로그 반복 읽기 대신 기존 상한과 변경 기반 조회를 지킨다.
5. 자유 문장의 질문을 물음표만으로 확정하지 않는다. 명시적 신호가 없는 새 최종 응답은 `완료·확인 필요`로 남기고, 실제 확인 후 `다음 지시 대기`로 전환한다. 이 경로는 답변 내용의 의미를 오판하지 않으면서 놓침을 줄인다.
6. 기존 작업 화면 계약을 보존한다. 큐가 비어도 `waiting_ui.render`는 매 프레임 호출하며, 세션 0 + 승인 1일 때 펼친 카드와 빈 상태 안내가 함께 나와야 한다.

## 재현 및 검증

- 독립 진단 실행 파일: `/tmp/deppy-agent-wait-audit-20260911/audit`.
- main의 `status.rs`, `agent_transcript.rs`는 path module로 직접 사용했고 `merge_agent_status`는 본문을 그대로 추출했다. 주변 agent 종류 enum만 stub으로 제공했다. 앱 전체 통합 검증이 아니다.
- 합성 fixture만 사용. 컴파일 exit 0, 9개 분류 조건 assertion 통과, 진단 exit 0.
- 로그: `/tmp/deppy-agent-wait-audit-20260911/compile.log`, `reproduction.log`.
- 실행 명령: `/tmp/deppy-agent-wait-audit-20260911/audit` (마지막 조건에서 11초 기다림).
- 전체 테스트·게이트·앱 패키징·재실행·UI 검증은 수행하지 않았다. 분류 결함을 재현했다는 뜻이지 수정된 동작의 PASS가 아니다.

## 출처

- [Claude 공식 Hooks](https://code.claude.com/docs/en/hooks): Notification matcher와 AskUserQuestion/ExitPlanMode.
- [Codex 0.154.0 질문 도구](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core/src/tools/handlers/request_user_input.rs).
- [Codex 0.154.0 도구 훅 연결](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core/src/tools/registry.rs).
- [Codex TUI 질문 훅 격차 보고](https://github.com/openai/codex/issues/19328): 보조 자료, 설치 버전 실측을 대체하지 않음.
- [Grok 공식 Hooks](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md). 로컬 공식 문서도 함께 대조.
