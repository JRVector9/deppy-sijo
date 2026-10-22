# Command Center 후속 PR1·PR2 설계

사용자는 xAegis 비교 후 제안한 여섯 단계 중 **PR1 메인 관리 도구·실제 대화 연결, PR2 실제 검증 실행**을 선택하고 서브에이전트 병렬 구현을 지시했다. 기존 승인한 Command Center 화면 구조를 확장한다. 이번 범위에 대한 구현 지시는 이미 있으므로 새 설계 승인 대기 없이 진행한다.

## 목표와 범위

- PR1: 정확히 연결한 프로젝트 메인이 조회·계획 제안·확인된 작업 배정을 실제 도구로 수행한다. MAIN에서 실제 사용자 입력·에이전트 답변·도구 활동을 확인한다.
- PR2: 사용자가 명령·실행 폴더·대상 결과를 확인해 로컬 검증을 실행한다. 앱이 관측한 종료 코드·출력·시간을 저장하고 수동 확인과 구분한다.
- 기존 task/attempt/result/config/runtime 식별, 계획 확인, 실행 권한, 최종 사용자 확인을 유지한다. 목표 자동 수행(PR3), 쉬운 기기 등록/새 공급자 어댑터(PR4), 결과 Git 통합(PR5), 자원 배정/모바일 원격 실행(PR6)은 추가하지 않는다.
- 앱 시작·종료·재시작, 릴리스 배포·버전 변경, 실제 사용자 세션에 테스트 입력, 유료 모델 요청, 원격 push/main merge는 이번 구현에 포함하지 않는다.

## PR1: 메인 도구와 실제 대화

기존 MCP HTTP 인증·요청 큐를 재사용하되 메인 전용 비공개 loopback 서버와 별도 자격 증명을 둔다. 자격 증명은 실행 준비 시 pending이며 정확한 메인 launch 결과·프로젝트·세션·runtime incarnation·execution generation·설정 revision을 확인한 뒤 활성화한다. 일반 공유/OAuth 연결은 관리 도구 권한을 얻지 않는다. 메인 교체·설정 변경·런타임 소실 때 회수한다.

도구는 `cc_get_project`, `cc_get_task`, `cc_propose_plan`, `cc_dispatch_task`, `cc_get_operation`이다. 조회는 범위가 제한되며 변경은 요청 goal/config revision과 기존 host admission을 재검사한다. 메인은 계획을 제안할 수 있으나 사용자 계획 확인을 대체하지 않는다. operation ID는 같은 내용의 원래 결과 조회에만 재사용하고, 관측 불명 입력/작업을 새 ID로 자동 재실행하지 않는다.

새 메인 전용 launch profile에만 MCP 주소와 secret environment를 등록한다. 실제 설치 CLI 설정으로 확인된 Codex 연결을 우선 구현한다. 작업자 profile과 기존 사용자 세션에는 자격 증명을 주입하지 않는다. 기존 메인을 자동 재시작하거나 공유 권한을 켜지 않는다. 지원되지 않는 provider/기존 세션은 도구 미연결 상태를 명확히 표시한다.

실제 대화 projection은 `Project.main_conversation: Option<MainConversation>`이다. `MainConversation`은 exact `RouteTarget`, 실제 generation, 순서 있는 `ConversationEntry`, truncated/status를 갖는다. entry는 안정된 ID, 실제 turn ID(있을 때), User/Agent/Tool, 본문, tool name/state, 실제 관측 시각(있을 때)이다. 최신100개·항목8KiB·총128KiB로 제한한다. 본문을 UTF-8 경계에서 자르고 시간·도구 결과를 만들어내지 않는다.

구조화된 App Server main은 현재 `AgentSession.items`를 읽기 전용으로 투영한다. PTY main은 앱에서 실제 접수한 CC 입력과 정확한 observation에서 얻은 답변/관리 도구 활동만 표시한다. CLI 원문 전체를 검색하거나 터미널 행을 임의 대화로 추측하지 않는다. 지원되지 않는 live stream은 원문 터미널로 이동할 수 있는 상태 설명을 표시한다. native와 PWA가 같은 bounded DTO를 사용한다.

## PR2: 검증 실행과 영속 증거

`VerificationTarget`은 workspace/task/attempt/result revision/source/artifact identity를 보존한다. `VerificationSpec`은 정확한 command/cwd/timeout/output limit이다. 승인 함수는 현재 local attempt의 실제 폴더와 요청 cwd의 canonical identity를 비교한다. source_revision은 현재 result의 식별 문자열이며 실제로 Git HEAD를 읽지 않았다면 commit이라고 표시하지 않는다.

백그라운드 worker가 Running을 sidecar에 먼저 저장한 뒤 `/bin/sh -c`로 실행한다. UI thread에서 파일·프로세스·DB 작업을 하지 않는다. 시간·출력·큐·증거 수에 상한이 있고 취소·timeout·worker 종료 시 소유한 자식 process group을 정리한다. 성공, nonzero, spawn error, timeout, cancel, interrupted, I/O 실패를 구분한다. output은 기존 redaction service로 처리한 뒤 저장하며 등록된 secret이 명령 본문에 포함되면 실행 전 거절한다.

새 evidence는 기존 metadata DB와 분리된 `command-center-v2.sqlite3`의 하위 호환 additive schema로 저장한다. 재시작 후 남은 Running은 Interrupted로 조회하며 실행을 재개하지 않는다. 결과 수신 시 exact current task/attempt/result/source/config/host generation을 다시 확인한다. 오래된 증거는 기록으로 보존하되 새 결과의 검증 판정에 적용하지 않는다. 성공 검증도 최종 사용자 완료 확인을 자동 수행하지 않는다.

native 작업 상세의 기존 결과·검증 영역에 command/cwd/제한과 실행·취소·증거 상세를 추가한다. 기존 수동 검증은 별도 레이블로 유지한다. PWA는 이 새 로컬 검증 실행 권한을 얻지 않으며 증거만 읽는다. 원격 결과에 대한 로컬 임의 명령 실행을 허용하지 않는다.

## 병렬 소유와 검증

- 메인 도구 agent: `crates/agent-mcp/**`, `command_center_main_tools.rs`, `command_center_launch.rs`.
- 검증 agent: core verification DTO, sidecar storage, `work_management/verification*.rs`, 최소 module exports.
- 화면 agent: `ui/command_center/**`, 관련 web rendering/tests.
- Root: 공통 `app.rs`/`command_center_host.rs`/`command_center_observe.rs` glue, 실제 대화 source getter/projection, 통합·리뷰·회귀 검증·공유 handoff.

각 agent는 GPT-6.1-Sol xhigh, 별도 worktree/Cargo target을 사용한다. 실제 실패하는 행동 테스트를 먼저 실행한 뒤 최소 구현한다. 인증·범위·stale result·timeout/취소·output cap·복구 및 native/PWA 표시를 독립 검토한다. 구현 커밋과 증거 문서는 PR1/PR2로 구분하고 전체 통합 상태에서 관련 suite·Clippy·format을 실행한다.
