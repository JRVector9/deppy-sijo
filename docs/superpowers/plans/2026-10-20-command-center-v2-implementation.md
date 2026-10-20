# 1안 발전형 에이전트 지휘 터미널 Implementation Plan

> **For agentic workers:** PR별 체크리스트를 순서대로 실행한다. 실행 시 사용 가능한 `execute-plan` 절차를 따른다. 병렬 에이전트 실행은 별도 요청이 있을 때만 하며 모델은 `gpt-6.1-sol`, reasoning effort는 `xhigh`다.

**Goal:** 채택한 HTML의 프로젝트 탐색·메인 에이전트·작업자 모니터를 네이티브 앱과 PWA에 적용하고, 실제 독립 작업 배정·실행·승인·결과 확인까지 연결한다.

**Architecture:** Rust/egui를 유지한다. 기존 Fleet/AgentSurface의 상태 투영, tracked input admission, 세션·이력·Git worker, TLS remote/Relay 권한을 재사용한다. 작업·배정의 영속 계약만 추가하고 UI는 읽기 전용 snapshot과 intent를 통해 host/controller에 연결한다. `storage-core`는 DB 기반 기능만 담당하며 작업 도메인을 넣지 않는다.

**Tech Stack:** 기존 Rust workspace, egui/eframe 0.36, SQLite/rusqlite, sync runtime worker, 기존 웹 원격 HTML/CSS/JS. 새 UI 프레임워크·별도 Node 서버·내장 HTML WebView를 추가하지 않는다.

---

## 현재 단계와 브랜치

2026-10-20 후속 실행 지시: 사용자가 서브에이전트 병렬 개발과 즉시 원안 복구를 요청했다. 원안 tag/branch와 기능 스위치를 유지한다. **PR2 저장은 기존 metadata DB migration 대신 별도 `command-center-v2.sqlite3` 보조 DB로 변경한다.** workspace 참조는 host가 기존 workspace ID로 검증하며, 보조 DB 내부 goal/task/attempt의 FK·unique·revision 계약은 유지한다. 이는 원안 앱의 DB schema 호환성을 보존하기 위한 변경이다. 아래 최초 계획의 전역 migration 지시는 이 수정으로 대체한다.

- 현재는 **승인된 계획의 병렬 구현·통합 검증 단계**다. 제품 변경은 통합 기능 브랜치에서 PR 범위별 로컬 커밋으로 관리하며, 실제 GitHub PR 생성/push/main 병합은 하지 않았다.
- 통합 브랜치: `feat/agent-command-center-v2-20261020`.
- 작업 폴더: `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`.
- 기준: `git fetch origin` 후 확인한 `origin/main`의 `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`.
- 별도 settings/cloud 작업 폴더의 미커밋 변경은 포함하지 않는다. 필요한 수정이 main에 병합되면 PR 경계에서 최신 main을 반영하고 관련 회귀를 다시 확인한다.
- 각 PR은 `feat/command-center-pr01-contracts`부터 `feat/command-center-pr09-release`까지 별도 브랜치로 만들고 **통합 브랜치 `feat/agent-command-center-v2-20261020`를 base로 한다**. 선행 PR이 이 브랜치에 병합된 뒤 다음 PR을 분기한다. 독립 PR의 선행 개발이 필요하면 stacked PR의 base를 명시한다. PR1~9 중간 결과를 main에 직접 병합하지 않는다.
- 통합 브랜치에서 최종 검토 후 main 대상 통합 PR을 준비한다. main 병합·출시는 별도 사용자 지시 범위에서 진행하며 이번 계획 보고에 포함하지 않는다.
- 후속 사용자가 병렬 개발을 승인했다. 제품 소스 수정과 검사 빌드는 진행하며 앱 실행·재시작은 하지 않는다.

## 고정한 화면 기준

HTML: `docs/mockups/multi-agent-command-center-v2-2026-10-20.html`.
설계: `docs/superpowers/specs/2026-10-20-command-center-v2-design.md`.
참조 이미지·SHA: `docs/design/command-center-v2/reference.json`.

| 영역 | 적용 기준 |
| --- | --- |
| 상단 | 메인 에이전트 / 작업 / 터미널 / 에이전트·기기 / 이력 |
| 왼쪽 | 프로젝트별 독립 세션 탐색, 프로젝트 클릭은 메인으로 이동 |
| 중앙 | 목표·배정·진행 취합·선택 작업 상세, 대상이 표시된 하나의 입력창 |
| 오른쪽 | 현재 프로젝트의 작업자·상태·진행·기기, 승인·결과·터미널 진입 |
| 넓은 화면 | 상단48, 프로젝트 머리글84, 좌측220, 우측312, 패널 머리글48, 버튼34 논리 단위 |
| 1051~1250 | 좌측196, 우측274. 본문 너비를 우선 확보 |
| 1050 이하 | 우측310 펼침 패널, 프로젝트 머리글78. 작업자 버튼으로 열기 |
| 760 이하 | 상단46, 프로젝트 머리글72, 본문 머리글44, 좌측260 펼침 패널, 우측은 min(330, 화면−32). 양 패널 동시 열기 방지 |
| 테마·입력 | 다크/라이트, 기존 글꼴 배율, IME/NFC, 비포커스 입력창 축소 유지 |

기본 배율에서는 HTML의 치수·간격·정렬을 대조한다. 글꼴 배율 증가 시 글자를 자르거나 축소해 맞추지 않고 줄바꿈·스크롤·접힘으로 대응한다. 화면이 같은 것과 실제 기능이 동작하는 것을 각각 검증한다.

## 실제 코드에서 확인한 재사용 근거

| 기존 파일 | 확인한 계약 | 적용 |
| --- | --- | --- |
| `crates/app/src/fleet.rs` | `FleetSession`, `FleetPromptTarget`의 workspace/runtime/execution identity | 우측 관측·정확한 대상 이동·입력 |
| `crates/app/src/agent_surface.rs` | `AgentCapabilities`, PTY와 AppServer 분리 | 미지원 승인·모델 제어를 활성화하지 않음 |
| `crates/runtime/src/command.rs` | `WriteInputBatchTracked`, `WriteInputTracked` | 긴 입력과 제출 경계 보존 |
| `crates/runtime/src/event.rs` | `InputAdmitted`, `SessionInputSubmitted` | 접수·실행·완료를 구분 |
| `crates/app/src/composer_drafts.rs` | workspace별 bounded draft checkpoint | 메인/작업의 독립 입력과 복구 |
| `crates/storage/src/db.rs` | `workspaces`, `agent_sessions`, `structured_threads`, `agent_work_turns` | 기존 프로젝트·세션·턴과 새 작업 identity 연결 |
| `crates/runtime/src/persistence.rs` | durable pane/session과 process-local session 분리 | 재시작 시 실행 중복 방지 |
| `crates/app/src/agent_work_git.rs`, `ui/work_history.rs` | Git worker·정확한 턴 이력 | 작업 상세와 기존 이력 연결 |
| `crates/runtime/src/remote.rs`, `web-remote/src/relay/` | TLS remote·기기 페어링·권한 | 연결 화면 재사용, 실행 장비는 별도 식별 |
| `crates/storage-core/src/lib.rs` | 전역 순서의 migration·사전 backup | 새 migration은 끝에만 추가 |

`agent_work_turns`는 provider 턴 이력이며 배정 DAG가 아니다. 기존 테이블을 작업 테이블로 재해석하지 않는다. Relay의 화면 열람/입력 권한도 원격 컴퓨터에서 새 프로세스를 실행할 권한으로 재해석하지 않는다.

## PR 요약과 의존 관계

| PR | 범위 | 사용자에게 생기는 결과 | 선행 |
| --- | --- | --- | --- |
| 0 | 계획·시안 기준 고정 | 브랜치와 비교 가능한 원본 확보 | 완료 |
| 1 | 작업 identity·상태 계약 | 같은 프로젝트/에이전트도 완전히 독립 | 0 |
| 2 | 저장·프로젝트 실행 설정 | 메인/작업/계획/설정/초안 복구 | 1 |
| 3 | 네이티브 3단 화면 | 시안의 상단·좌측·중앙·우측 적용 | 1,2 |
| 4 | 로컬 배정·입력·worktree | 메인/작업자 입력과 실제 독립 실행 | 1,2,3 |
| 5 | 메인 계획·조율·결과 취합 | 목표에서 역할별 분담과 후속 검증 | 4 |
| 6 | 승인·결과·Git·기록 | 작업 안에서 개입하고 근거 확인 | 4,5 |
| 7 | 기기 연결·원격 작업 배정 | 기기별 프로젝트 연결과 작업 분담 | 4,5,6 |
| 8 | PWA 동일 흐름 | 휴대폰에서 메인·작업·결과 조작 | 3,6,7 |
| 9 | 통합 검증·성능·출시 | 회귀 검증과 버전이 구분되는 빌드 | 1~8 |

PR3의 UI leaf와 PR4의 dispatch 내부는 PR1/2 계약 확정 후 분리 개발할 수 있다. `app.rs`, 공용 타입, migration 원장, locale 통합은 한 담당자가 순차 반영한다. 최종 연결과 완료 판정은 위 의존 순서를 따른다.

## PR1 — 작업 identity와 상태 계약

**추가:** `crates/core/src/work_management.rs`, `crates/app/src/work_management/{mod,reducer,tests}.rs`.
**수정:** `crates/core/src/lib.rs`, `crates/app/src/main.rs`의 module 선언만.

- [ ] 기존 `WorkspaceId`를 프로젝트 identity로 재사용한다. 이름·경로·provider 조합을 새 프로젝트 key로 사용하지 않는다.
- [ ] `GoalId`, `TaskId`, `AttemptId`, `SubmissionId`, `ConfigRevision`을 구분한다. `TaskId`는 작업, `AttemptId`는 해당 작업의 재시도, `SubmissionId`는 신규 사용자 전송을 식별한다.
- [ ] 영속 대상은 pane/provider thread 식별자, 실행 중 대상은 runtime incarnation + session + execution generation으로 분리한다. runtime session 번호만 저장해 재사용하지 않는다.
- [ ] 상태는 계획 확인/대기/실행/승인/사용자 입력 대기/결과 제출/검증 실패/사용자 확인/실패/취소/관측 불명으로 구분한다. timeout은 완료가 아니라 관측 불명이다.
- [ ] 순수 reducer에서 이전 attempt·이전 generation·중복 event를 무시한다. 선행 작업 결과의 revision과 명시적 검증 결과를 확인해야 후속 실행이 가능하다.
- [ ] 작업 단위 입력 상한은 기존 Fleet와 같은16KiB로 공용 계약에 정의한다. 초안은 기존1MiB/총32MiB/256개 제한을 유지한다. 계획은 최대64작업, 이벤트 표시 payload는8KiB, 페이지는100행으로 제한하는 초기 계약을 테스트한다. 초과는 잘라서 전송하지 않고 명시적으로 거부하며 원문 초안을 보존한다. 실행 중 참조를 지우는 자동 보관 정리는 추가하지 않는다.
- [ ] 아래 소유권·중복·역순 fixture를 먼저 실패시키고 reducer를 구현해 통과시킨다.

| 입력 fixture | 필수 판정 |
| --- | --- |
| 동일 workspace/provider의 A·B, A 이벤트 수신 | B 제목·상태·초안 불변 |
| 같은 문장 두 번 신규 제출 | SubmissionId와 GoalId가 각각 다름 |
| 동일 operation 재시도·중복 ACK | 새 attempt·작업 생성 없음 |
| old attempt 완료가 새 attempt 실행 뒤 도착 | 새 attempt 상태 불변 |
| parent 종료 후 child 계속 실행 | 전체 완료로 표시하지 않음 |
| 연결 끊김 후 과거 완료 도착 | 정확한 실행에만 결과 연결 |

**검증 명령:** `cargo test --offline --locked -p deppy-core work_management`; `cargo test --offline --locked -p deppy-sijo work_management`.
**완료 기준:** 해당 회귀가 통과하고 기존 provider 상태 계층을 대체하지 않는 계약 PR로 리뷰 가능하다. 아직 UI·프로세스 실행은 추가하지 않는다.

## PR2 — 영속 저장·프로젝트 설정·초안

**추가:** `crates/storage/src/work_management.rs`, `crates/app/src/work_management/{repository_worker,settings}.rs`.
**수정:** `crates/storage/src/{db,lib}.rs`, `crates/app/src/{composer_drafts,composer_draft_worker}.rs`.

- [ ] 기존 Db의 path module 패턴으로 저장 모듈을 연결한다. `Db` private connection을 UI에 공개하지 않는다.
- [ ] 신규 `work_project_configs`, `work_goals`, `work_tasks`, `work_attempts`, `work_dependencies`, `work_events`, `work_dispatch_outbox`를 전역 migration 끝에 추가한다. 아래 소유권 제약을 migration 테스트로 고정한다.

| 데이터 | 소유권·제약 |
| --- | --- |
| config | workspace FK, CAS revision, 메인/작업자/한도/자동화 설정 |
| goal | workspace+goal PK, workspace+submission UNIQUE |
| task | workspace+task PK, 같은 workspace의 goal FK |
| attempt | workspace+attempt PK, task+attempt 번호 UNIQUE, 배정 당시 설정·폴더 snapshot |
| dependency | 같은 workspace의 두 task FK, self/cycle 거부 |
| event | workspace와 정확한 task/attempt 연결, bounded 표시 필드 |
| outbox | attempt+operation UNIQUE, durable 접수와 ACK 상태 연결 |

- [ ] 기존 세션과의 연결에만 `agent_sessions`/`structured_threads`를 사용한다. 과거 `agent_work_turns`를 제목 유사성으로 새 task에 병합하지 않는다.
- [ ] 프로젝트 설정은 이름/폴더/메인/model, 펼침 실행 규칙은 effort/작업자/동시 한도/자동화/worktree로 나눈다. 현재 UI 한도1~8과 맞추고 실제 provider model/effort capability만 선택 가능하게 한다.
- [ ] 메인 binding과 작업자 binding, 메인/작업별 초안은 분리한다. 설정 변경은 새 attempt에 적용한다. 새 프로젝트에 이전 작업·초안·선택 탭을 복사하지 않는다.
- [ ] 저장 실패 시 초안을 유지하고 실패를 입력창 안에 표시한다. DB/파일 I/O는 bounded worker에서 처리한다.
- [ ] migration prefix 전체 업그레이드·FK·동시 CAS·중간 실패 rollback·설정 snapshot·복구 후 초안 격리 검사를 실행한다. fixture DB만 사용한다.

**검증:** `cargo test --offline --locked -p storage work_management`; `cargo run --offline --locked -p xtask -- smoke-db-migrations`; `cargo test --offline --locked -p deppy-sijo composer_drafts`.
**완료 기준:** 재기동용 snapshot roundtrip이 손실 없이 통과한다. 복원은 상태/초안 복원이며 에이전트 자동 재실행·불확실한 입력 재전송이 아니다.

## PR3 — 시안의 네이티브 3단 화면

**추가:** `crates/app/src/ui/command_center/{mod,layout,main,workers,settings,tests}.rs`.
**수정:** `crates/app/src/ui/{mod,agent_terminal,file_tree}.rs`, `crates/app/src/app.rs`의 화면 조합/intent 라우팅, `crates/i18n/locales/*/messages.txt`.

- [ ] 상단5개 메뉴·프로젝트 머리글·좌측 workspace/session·중앙 메인·우측 worker를 시안 치수로 구현한다. 거대한 `app.rs`에 렌더 본문을 추가하지 않는다.
- [ ] 기존 실제 세션 snapshot과 새 저장 데이터만 그린다. 우측에는 선택 프로젝트만 표시하고 전체/확인 필요/실행 중 필터를 적용한다.
- [ ] 프로젝트 클릭→해당 메인, 세션 클릭→정확한 pane/structured session, 우측→해당 작업 상세/터미널을 연결한다. 일반 터미널·파일트리·문서·Git 탭 사용 경로를 보존한다.
- [ ] 대상이 명시된 입력창을 연결한다. 비포커스 축소·포커스 확대 때 터미널 viewport/scrollback 보존 검사를 추가한다.
- [ ] 좌우 접기·좁은 창·다크/라이트·글꼴 배율·키보드 focus/Escape·AccessKit를 검증한다. 설정은 공용 field/notice/footer를 재사용하고 팝업 추가 시 `docs/design/popup-components.md`를 따른다.
- [ ] 현재 연결 가능한 기능만 활성화한다. 실행 기능이 아직 연결되지 않은 단계는 실제 빈/대기/미지원 상태를 보여주며 예시 실행·가짜 성공을 넣지 않는다.

**검증:** `cargo test --offline --locked -p deppy-sijo command_center`; headless egui 1440/1280/1000/760/390 논리 너비와 font scale1.0/1.25 fixture.
**완료 기준:** 시안과 요소·치수·대상 라우팅 비교표가 있고 기본3영역·두 drawer·양 테마 render evidence를 확보한다. 실제 실행은 PR4 완료로 따로 판정한다.

## PR4 — 로컬 배정·입력·대기열·worktree

**추가:** `crates/app/src/work_management/{dispatch,queue,delivery,worktree_owner}.rs`.
**수정:** `crates/app/src/{app,worktree}.rs`, 필요한 경우 `crates/runtime/src/{command,event,in_process}.rs`.

- [ ] 메인 한 세션과 독립 작업자 세션을 각각 생성/연결한다. typed assignment는 목표/범위/완료 조건/설정 revision/담당/실행 기기를 포함한다.
- [ ] outbox 저장→runtime 전달→`InputAdmitted`→provider 실행 관측을 각각 기록한다. 신규 전송은 항상 새 submission, retry만 같은 operation을 사용한다. 입력 거부·ACK 유실·timeout은 초안 또는 delivery-uncertain으로 남긴다.
- [ ] 기존 `WriteInputBatchTracked`의 paste/Enter 경계와 bootstrap 준비 상태를 재사용한다. scrollback 위치와 무관하게 입력하고 한글 IME/NFC/긴 문장/동일 문장 재전송을 검증한다.
- [ ] 동일 operation의 전달 중복을 worker/host 경계에서 거부한다. 프로세스 시작 뒤 ACK가 유실되면 실행 identity를 조회·대조하고 확인 전 새로 실행하지 않는다.
- [ ] 동시 한도·작업 의존·공유 파일 범위·독립 worktree 소유권을 적용한다. 리뷰 대상과 검증 대상은 동일 source/base와 결과 revision을 명시한다.
- [ ] sleep/wake·draining·재접속은 기존 attempt와 연결한다. worker가 이미 작업 중이면 메인 연결이 끊겨도 유지한다. 앱 복원만으로 과거 agent나 불확실한 명령을 자동 시작하지 않는다.

**검증:** `cargo test --offline --locked -p deppy-sijo work_management`; `cargo test --offline --locked -p deppy-sijo ime_input_harness`; `cargo test --offline --locked -p runtime input`; `cargo test --offline --locked -p deppy-sijo worktree`.
**완료 기준:** 같은 프로젝트에서 두 작업자가 별도 세션/폴더/초안으로 실행되고 입력이 정확한 대상에 한 번 전달된다. 종속 검증이 선행 결과 전에 실행되지 않는다.

## PR5 — 메인 에이전트 계획·분담·취합

**추가:** `crates/app/src/work_management/{orchestrator,plan,provider_adapter}.rs`.
**수정:** `crates/app/src/{codex_app_server,agent_session}.rs`, `ui/command_center/main.rs`.

- [ ] 메인 목표→계획 제안→사용자 확인→배정→결과 취합→검증의 상태 전이를 실제 provider 응답에 연결한다. HTML의 고정3역할 데이터를 제품 코드에 복사하지 않는다.
- [ ] 계획은 bounded schema로 검증하는 데이터다. provider가 제안한 경로/agent/기기/명령이 사용자 허용 범위를 확대하지 못하도록 host에서 검사한다. 검증에 실패한 계획은 자동 실행하지 않는다.
- [ ] 구현·독립 리뷰·통합 검증을 기본 제안하되 목표에 따라 배정안을 편집한다. 리뷰/검증이 참조한 source revision과 산출물 revision을 연결하고 오래된 리뷰를 새 구현의 승인으로 사용하지 않는다.
- [ ] 설정의 배정 전 확인을 기본으로 한다. 자동 진행은 이미 허용된 범위에서만 수행한다. 메인 planner의 도구 실행 제한은 adapter가 실제로 보장하는 범위만 사용한다.
- [ ] Codex AppServer와 PTY provider의 기능 차이를 드러낸다. 구조화된 계획을 지원하지 않는 연결에서는 실제 대화를 유지하고 수동 배정 편집을 제공한다. 이를 AI 자동 분담 완료로 표시하지 않는다.
- [ ] Codex/Claude/Kimi/Grok을 기존 capability에 맞춰 fixture 매트릭스로 검증한다. Cursor는 현재 `AgentProvider`에 없는 것을 확인했으므로 추가 adapter/capability 확인 없이 완전 지원이라고 표시하지 않는다. 사용자 등록 CLI의 터미널 사용 경로는 유지한다.

**검증:** `cargo test --offline --locked -p deppy-sijo work_management`; `cargo test --offline --locked -p deppy-sijo agent_surface`; `cargo test --offline --locked -p deppy-sijo agent_session`.
**완료 기준:** 메인의 한 목표에서 생성·확인한 복수 작업이 실제 배정되고 각 결과가 정확한 계획으로 돌아온다. 지원하지 않는 자동화와 미관측 상태가 표시된다.

## PR6 — 작업 안의 승인·결과·Git·기록

**추가:** `crates/app/src/ui/command_center/{approval,results,history,git}.rs`.
**수정:** `crates/app/src/{agent_actions,agent_work_git}.rs`, `ui/work_history.rs`, 정확한 host intent 연결부.

- [ ] 명령·기기·현재 작업 폴더·허용 범위를 함께 보여주고 원래 승인 owner로 허용/거절 intent를 보낸다. PTY에 임의 y/Enter를 보내 공통 승인 API인 것처럼 처리하지 않는다.
- [ ] 승인 key에 정확한 request/attempt/generation을 결합한다. 이전 승인, 다른 작업 승인, 완료된 요청을 현재 요청에 적용하지 않는다.
- [ ] 결과 제출/테스트 통과·실패·미실행/메인 검토/사용자 확인을 각각 표시한다. 재작업은 결과 revision을 바꾸며 후속 검증을 무효화한다.
- [ ] 작업과 결과/원문 터미널/Git 변경/기록 탭을 기존 worker와 연결한다. 다른 작업의 worktree diff나 provider thread를 따라가지 않는다.
- [ ] 프로젝트 전체 기록은 정확한 작업 링크를 포함하고 기존 턴 이력의 원문 탐색을 보존한다. 제목에는 내부 태그·tool payload를 사용하지 않는다.

**검증:** `cargo test --offline --locked -p deppy-sijo command_center`; `cargo test --offline --locked -p deppy-sijo work_history`; `cargo test --offline --locked -p deppy-sijo agent_work_git`; `cargo test --offline --locked -p deppy-sijo fleet`.
**완료 기준:** 사용자가 승인·결과 확인·보완 요청·Git·원문·기록을 정확한 작업 안에서 수행한다. 작업자 실행 종료만으로 프로젝트를 완료 처리하지 않는다.

## PR7 — 기기 연결과 원격 작업 배정

**추가:** `crates/app/src/work_management/{devices,remote_dispatch}.rs`, `crates/storage/src/work_devices.rs`, 필요한 worker 계약은 `crates/core/src/work_management.rs`에 추가.
**수정:** `crates/runtime/src/remote.rs`, `crates/web-remote/src/relay/`, `crates/app/src/ui/command_center/`의 기기 화면. DB migration 원장에 실행 기기 registry·project folder mapping을 append.

- [ ] 컴퓨터의 실행 worker 등록과 휴대폰/클라우드의 관찰·입력 연결을 구분한다. 기존 Relay 페어링·TLS pin·인가 만료/회수를 재사용한다.
- [ ] 기기마다 설치·로그인된 agent/capability, 프로젝트 폴더 anchor, 연결 freshness, capacity를 확인한 뒤 배정 후보로 표시한다. 폴더 매핑이 없거나 오프라인이면 새 배정을 막는다.
- [ ] 실행 권한은 기존 view/input 권한에 자동 포함하지 않는다. 사용자에게 실행 기기·폴더·허용 agent를 확인 가능하게 하고 그 범위만 저장한다.
- [ ] attempt 소유권·lease epoch·중복 operation을 확인한다. 연결 끊김 때 다른 장비로 작업을 몰래 재실행하지 않는다. 늦은 결과는 해당 attempt에만 연결한다.
- [ ] 재연결 reconcile, TLS pin 변경, 경로 이동, revoked device, slow peer와 bounded 큐를 테스트한다. 과거 결과는 보존하고 실시간 상태와 구분한다.

**검증:** `cargo test --offline --locked -p runtime remote`; `cargo test --offline --locked -p web-remote relay`; `cargo test --offline --locked -p storage work_devices`; `cargo test --offline --locked -p deppy-sijo work_management`.
**완료 기준:** 두 실행 worker fixture에서 장비·폴더별 배정/장애 복구/중복 차단이 통과하고 실제 허용된 두 기기에서 선택 배정 시나리오를 확인한다. SSH 접속 성공만으로 완료 판정하지 않는다.

## PR8 — PWA에 같은 정보 구조 적용

**추가:** `crates/web-remote/src/work_management.rs`, `crates/web-remote/tests/command_center_chrome.rs`, 해당 `tests/fixtures/` 시나리오.
**수정:** `crates/web-remote/src/{protocol,ws_api,dashboard}.rs`, `crates/web-remote/assets/{app.js,app.css,index.html,sw.js}`, `crates/app/src/cloud_agent/{access,control,completion_bridge}.rs`의 필요한 연결만.

- [ ] 동일 project/task/attempt snapshot과 intent 계약을 사용한다. 별도 모바일 작업 저장소를 만들지 않는다.
- [ ] 390/430 너비와 가로 모드에서 프로젝트/작업자 drawer, 메인/작업 상세, 하나의 입력창을 제공한다. 본문 가로 이동과 drawer 동시 열기를 막는다.
- [ ] 터미널은 사용 가능한 세로 영역을 채우고 과거 기록·현재 위치·키보드·안전영역을 유지한다. 작성 전용 별도 화면을 추가하지 않는다.
- [ ] 읽기/입력/승인/완료 알림은 기존 권한별로 처리한다. 허용되지 않은 프로젝트·task 정보는 서버 snapshot에서 배제한다. 미지원 provider의 완료 알림을 성공으로 표시하지 않는다.
- [ ] 목록과 선택 상세의 delta를 분리한다. 전체 transcript polling·숨긴 패널 반복 가공을 피하고 slow receiver의 retained bytes를 제한한다.

**검증:** `cargo test --offline --locked -p web-remote --test command_center_chrome`; `cargo test --offline --locked -p web-remote --test pwa_unified_chrome`; 기존 모바일 fixture 중 변경 영향에 해당하는 suite.
**완료 기준:** 허용된 휴대폰에서 메인 지시→배정 확인→작업 결과까지 같은 흐름으로 수행한다. IME/긴 입력/가로폭/키보드 표시/재접속과 권한 회수를 실제 화면에서 확인한다.

## PR9 — 통합·성능 검증과 버전이 구분되는 빌드

**추가:** `docs/qa/2026-10-20-command-center-v2-release.md`, 작업별 회귀 fixture와 최종 evidence.
**수정:** `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, 사용 문서·handoff, 검증으로 발견한 해당 모듈의 결함.

- [ ] 각 PR의 회귀를 유지한 채 전체 시나리오를 검증한다. 테스트 추가를 마지막 PR로 미루지 않는다.
- [ ] 실제 네이티브 화면을 시안과 대조한다. 상단/프로젝트 머리글/좌우 폭/행간/버튼/scroll/focus/테마/글꼴 배율을 각각 확인하고 차이를 수정한다.
- [ ] 동일 workspace/provider 다중 세션, 부모·하위 작업 순서, stale 승인/완료, 동일 문장 신규 제출, 초기 bootstrap/긴 한글/scrollback, sleep/wake/draining, reconnect/권한 회수/DB 실패를 통합 harness로 실행한다.
- [ ] 같은 빌드 조건에서 기존 main과 비교해 idle CPU, retained memory, frame p95, 전환 시간, queued/retained bytes, slow remote의 backpressure를 측정한다. 동일 부하의 반복 범위 밖 회귀는 원인을 확인하고 제거한다. 화면을 숨긴 뒤에도 새 polling/worker가 남지 않는지 확인한다.
- [ ] 새 기능 출시 버전은 잠정 `0.14.0`이다. 실제 배포 직전에 모든 최신 shipped 버전과 비교해 더 큰 버전으로 확정하고 workspace 상속 버전을 유지한다. 이 계획 단계에서 버전을 올리지 않는다.
- [ ] release binary의 `--version`과 bundle의 두 version 필드, 서명·아카이브를 검증한 뒤에만 빌드 전달 완료로 보고한다.
- [ ] 앱 종료/재실행은 해당 단계의 현재 사용자 요청에 있을 때만 한다. 이전 요청으로 자동 재실행하지 않는다. 실행이 필요한 실화면 확인은 그 시점의 명시적 실행 요청과 연결한다.

**공통 명령(구현 후 실행할 검증이며 이번 계획 단계에서는 미실행):**

```sh
export CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target
export CARGO_BUILD_JOBS=2
cargo fmt --all -- --check
cargo test --offline --locked --workspace
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
cargo run --offline --locked -p xtask -- check-boundary
cargo run --offline --locked -p xtask -- check-deps
cargo run --offline --locked -p xtask -- i18n-check
git diff --check
```

offline 실행이 의존성 미설치로 실패하면 환경 실패와 코드 실패를 분리하고 의존성 확보 후 재실행한다. 실패/skip/미실행을 pass에 합산하지 않는다. 서명/패키징은 기존 `scripts/package-macos.sh`, `scripts/verify-macos-package.sh` 계약을 사용하고 신뢰 검사를 임의로 완화하지 않는다.

## PR마다 적용하는 완료 기준

1. 변경 범위에 맞는 실패 재현 또는 동작 검증 → 구현 → 해당 회귀 통과.
2. 직접 diff review와 필요 시 명시적 CLI review. CLI 호출 시 반드시 `gpt-6.1-sol`/`xhigh`.
3. UI 변경은 native render/상호작용 evidence, host 변경은 exact identity·실패 주입 evidence를 첨부한다.
4. `docs/CODEX_HANDOFF.md`에 수정 파일·실제 명령/결과·실패 접근·남은 일을 기록한다.
5. PR별 기능/검증 범위를 설명하는 커밋으로 분리한다. 선행 PR 통과 후 통합 브랜치로 병합하며, main에는 중간 PR을 직접 반영하지 않는다.

## 최종 인수 시나리오

- 새 프로젝트 설정 → 메인 연결 → 목표 입력 → 실제 배정안 확인 → 구현/리뷰를 독립 실행 → 선행 결과에 맞춰 검증 → 결과/Git/원문 확인 → 사용자 확인.
- 중앙 메인 대화 중 오른쪽 상태가 갱신되고, 우측에서 정확한 승인/작업자 터미널로 이동한다.
- 같은 프로젝트의 같은 provider 세션 두 개가 제목/작업/초안/문서·Git 탭/완료 상태를 공유하지 않는다.
- 메인 연결이 끊겨도 이미 실행 중인 작업은 유지되며 재연결·앱 복구가 새 중복 실행을 만들지 않는다.
- 다른 컴퓨터에 정확한 폴더·agent로 작업을 배정하고 휴대폰에서 허용한 작업만 확인·입력한다.
- 네이티브와 PWA에서 미지원 기능·현재 상태 미확인·실패·미실행을 완료/성공과 구분한다.

시안이 맞는 PR3 완료와 전체 서비스 동작이 맞는 PR1~9 완료는 별도 이정표다. UI만 적용한 상태를 멀티 에이전트 기능 전체 완료로 보고하지 않는다.
