# Pane 복원과 프로젝트 API 사용 개선 개발 계획

**상태:** 2026-10-23 계획을 사용자 “진행해” 승인 후 구현했다. 2026-10-24 요청한 병렬 코드 리뷰와 발견 결함 보완·최종 검증을 마쳤다. 앱 배포·재시작은 하지 않았다. 실제 결과는 [구현·검증 보고서](../../qa/2026-10-24-pane-project-api-implementation-review.md)에 기록한다.

**Goal:** 분할 종료 후 과거 터미널 출력의 폭을 올바르게 복원하고, 프로젝트 키 폼을 조밀하게 정리하며, 우클릭에서 바로 API를 등록하고 에이전트가 현재 프로젝트의 사용 가능한 API를 발견·사용하게 한다.

**Architecture:** 기존 terminal resize/reflow와 프로젝트 키 보관·CAS·worker·실행 snapshot을 재사용한다. UI는 공용 팝업과 독립적인 직접 등록 host를 사용한다. 에이전트에는 값 없는 검색 목록과 범위가 제한된 사용 경로를 제공한다. 신규 저장소나 일반 MCP 프록시의 도구 구현을 중복해서 만들지 않는다.

**Tech Stack:** Rust, egui/egui_kittest, 기존 alacritty streaming reflow, SQLite/secret store, project_keys worker/CLI, agent-mcp, 기존 runtime/SSH.

## 작업 기준과 범위

- canonical: `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`
- branch: `fix/product-review-integration-20261023`, HEAD `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1` + 기존 미커밋 변경. nominal `deppy-sijo`의 다른 작업 트리를 수정하지 않는다.
- 최초 작성은 읽기 전용 개발 계획이었다. 이후 승인된 제품 구현과 테스트를 진행했다. 앱 재시작·배포는 이번 구현 승인에 포함되지 않는다.
- 구현 착수 시 현재 작업 상태의 기준 스냅샷을 보존하고, 같은 기준에서 작업 단위를 분리한다. 기존 SSH/launcher/usage 수정과 무관한 파일을 되돌리거나 PR에 섞지 않는다.
- 모든 서브에이전트는 `gpt-6.1-sol`, `xhigh`. root를 포함해 동시 4명까지 사용한다.
- 사용량 보호 파일/영역·테스트·manifest는 이번 변경 대상이 아니다. 원본 seal을 바꾸지 않고 source guard로 확인한다.
- 변경 앱을 나중에 배포/사용하도록 빌드할 때는 기존 배포0.18.0보다 높은 버전으로 올리고 artifact 버전을 검증한다. 재실행은 별도 요청이 있어야 한다.

## 최종 실행 결과 — 2026-10-24

| PR 단위 | 결과 | 남긴 경계 |
|---|---|---|
| 1 | 실제 split-close/history 회귀 5건 통과 | 원래 증상은 재현되지 않았다. terminal 제품 코드는 추측 수정하지 않음 |
| 2 | 조밀한 공용 키 폼·오류 수명·CAS reload 구현 | 합성 offscreen/geometry 검증. 실행 중 앱의 수동 화면 검사 아님 |
| 3 | 메모·터미널 API 바로 등록·전역 host·입력 fence 구현 | SSH는 지원 불가 안내. 파일트리는 메모 요청을 전달함 |
| 4 | metadata catalog·명시 grant·세션 bearer·원자적 최신 권한 검증 구현 | Mac 보관함의 원격 전송, 일반 Cloud 권한 자동 승격 없음 |
| 5 | 새 로컬 Claude/Codex/Cursor/Grok 발견·selected-key 실행 구현 | Kimi additive adapter·Cloud recipe bridge·기존 세션 bootstrap/만료 갱신 미지원 |
| 6 | 세 독립 리뷰·발견 결함 보완·최종 전체 5,359 PASS | 56 ignored 별도. 실제 commit/push/PR 생성·빌드 배포는 하지 않음 |

Strict Clippy, fmt, boundary, usage source guard와 diff check도 실제 통과했다. [최종 보고서](../../qa/2026-10-24-pane-project-api-implementation-review.md)가 현재 상태와 검증 증거의 기준이다. 아래 상세는 최초 승인 계획과 완료 기준을 보존한 것이며, 지원 여부는 이 결과 표와 보고서를 따른다.

## 소스에서 확인한 현재 상태

| 요구 | 이미 있는 것 | 이번에 확인/추가할 것 |
|---|---|---|
| 분할 종료 폭 복원 | `workspace.rs`에서 pane 가용 폭→cols 계산, backend primary soft-wrap/history streaming reflow, tracked resize ACK | 실제 sibling 닫기와 과거 scrollback을 함께 검증하는 회귀; 실패한 전달/보관/표시 층만 수정 |
| 키 편집 폼 | 480pt 공용 팝업, 36pt 입력·34pt 버튼, 모든 필드 세로 배치 | 조밀한 2열 배치, 편집기 전용 간격, 화면별 no-scroll 치수 검증 |
| 우클릭 등록 | terminal 선택→EnvironmentOpenRequest→환경설정, API 자유형 이름/값의 legacy 등록 경로 | 설정 이동 없는 직접 등록 host; 메모 메뉴의 새 진입점 |
| 에이전트 키 사용 | LegacyAgent 새 프로세스 env 주입, `env run` 최신 원본·scope·snapshot·PEM·receipt | 값 없는 검색, 모델이 실제 읽는 발견 경로, 명시 키 선택·별도 사용 grant |

이번 조사에서는 실행 중 증상을 재현하지 않았다. 분할 문제의 원인은 아직 확정하지 않았다. 이미지의 Stale도 실제 CAS 충돌인지 이전 오류가 새 양식에 남은 것인지 재현 후 구분한다.

### 설계 선택

추천은 **기존 보관함/실행 서비스를 재사용하고 탐색·등록 동선을 얹는 안**이다. 별도 간편 저장소는 키 회전·이력·프로젝트 scope가 이중화되므로 채택하지 않는다. 환경변수 주입만 추가하는 안은 기존 프로세스의 값 갱신과 모델의 키 발견을 해결하지 못하므로 단독 해법으로 채택하지 않는다.

## PR 구성과 병렬 순서

| PR | 담당 | 결과 | 의존성 |
|---|---|---|---|
| 1 | A: terminal | 분할 종료 후 최신 폭/과거 soft-wrap 이력 회귀와 원인 수정 | 독립 |
| 2 | B: UI | 키 추가·수정 폼의 조밀한 배치와 오류 수명 분리 | 독립 |
| 3 | B + root 접합 | 메모·터미널 우클릭→현재 프로젝트 API 바로 등록 | PR1의 workspace 변경 완료 + PR2 편집기 계약 |
| 4 | C: key service | API metadata 검색·추가형 agent 사용 grant 계약 | 독립 |
| 5 | C + root 접합 | 현재 세션의 자동 발견, 최신 허용 키로 실행, MCP adapter | PR4 |
| 6 | A/B/C 검증 + root | PR1~5 연결, 실제 화면·세션 격리·회전·SSH 회귀 통합 | PR1~5 |

1차 병렬: **A→PR1 / B→PR2 / C→PR4**, root가 공유 계약과 App 접합 준비.
2차 병렬: **B→PR3 / C→PR5 / A→terminal·SSH 통합 검증**. PR1을 적용한 뒤 PR3의 workspace 변경을 시작한다.
마지막: root가 PR별 결과와 상호작용을 검토하고 PR6에서 통합 완료 기준을 확인한다.

### 충돌 방지 소유권

- A만 terminal/backend/renderer/runtime resize 테스트를 수정한다. PR1 동안 `ui/workspace.rs`도 A 소유다.
- B만 project-key 편집기·notes UI·등록 양식을 수정한다. PR3의 `workspace.rs` 변경은 PR1 완료 뒤 root가 적용한다.
- C만 core/storage의 API metadata/grant, project-key 검색·실행 서비스, MCP 도구 계약을 수정한다.
- **`app.rs`, 모듈 등록, 번역 5개 언어, 공용 시안 HTML, 디자인 문서, Cargo 변경·실행은 root만 수정/수행한다.** root의 접합 변경도 PR3→PR5 순으로 적용한다.
- PR2/PR3/PR5가 같은 편집기 파일을 병렬 수정하지 않는다. agent-use UI는 PR4 계약이 고정된 후 B가 추가하고 PR5가 소비한다.
- 서브에이전트는 구현 차이와 요청한 테스트를 제출한다. root가 Cargo를 직렬 실행하며, 동일 target의 중복 빌드와 작업 중 파일 변경을 피한다.

## PR1 — 분할 종료 후 과거 출력 폭 복원

**대상 파일:**
`crates/app/src/ui/workspace.rs`, `crates/app/src/workspace_ui_test_support/workspace_table_resize_tests.rs`,
`crates/terminal/src/{alacritty_backend.rs,renderer_egui.rs}`, `crates/session/tests/terminal_table_resize.rs`.
실제 실패 근거가 있을 때만 `crates/runtime/src/{in_process.rs,remote/ssh_stream.rs}` 또는
`third_party/alacritty_terminal-0.26.0/src/grid/streaming_resize.rs`를 수정한다.

- 실제 2-pane mux에서 좁은 쪽에 긴 문장·400행 이상 history를 출력하고 sibling을 닫는 회귀를 먼저 추가한다. 단순 가용 폭 함수 검사로 대체하지 않는다.
- 새 출력 없이 survivor의 목표 cols, resize admission/ACK, 적용된 snapshot.cols, history 행과 최종 paint를 비교해 실패를 재현한다.
- 실패 위치에 따라 최소 수정한다: 최신 목표 미전달이면 resize/fence 수명, backend는 맞고 화면만 이전 상태면 폭/history 세대에 따른 캐시 무효화, 논리 행 경계가 사라졌으면 backend/archive WRAPLINE 보존을 수정한다. 정상인 층에는 별도 reflow를 추가하지 않는다.
- ASCII·한글·결합문자·emoji, 압축 history, 위로 스크롤, archive 왕복, 반복 split/close, 진행 중 resize와 늦은 ACK, SSH 재연결을 검사한다.
- 선택·복사·검색·cursor·SGR 스타일이 유지되는지 확인한다.

**완료 기준:** 자동 줄바꿈된 과거 출력이 새 폭에 맞게 재배치되며 새 데이터가 오기 전에도 이전 좁은 snapshot이 남지 않는다. 명시적 CRLF와 alternate-screen TUI의 셀 의미는 보존한다. 표가 명시적 개행으로 그려졌다면 과거 행을 추측해서 합치지 않고 현재 TUI의 실제 resize/redraw 경로를 검증한다.

**기존 회귀:** `terminal_reflows_soft_wrapped_text_but_preserves_explicit_table_columns`,
`pane_resize_reaches_foreground_pty_and_redraws_table_at_each_width`, `table_resize_*`,
`streaming_resize_matches_stock_for_unicode_cursor_and_scrollback`.

## PR2 — 프로젝트 키 폼 축소

**대상 파일:** `crates/app/src/ui/project_keys/{dialogs.rs,tests.rs}`, `crates/app/src/ui/project_keys.rs`.
필요한 편집기 분리 파일은 `crates/app/src/ui/project_keys/editor.rs`로 한정한다.
공용 density 확장이 필요할 때만 `crates/connector-ui/src/popup/{shell.rs,fields.rs}`에 opt-in으로 추가한다.

- 기존 geometry 테스트를 실제 본문 콘텐츠와 clip 높이를 비교하도록 강화하고, 넘치는 상태를 먼저 재현한다.
- 폭560pt, 입력36pt, 버튼34pt 유지. 필드 행 간격12pt·열 간격12pt를 한 공통 상수/옵션으로 적용한다. 다른 팝업의 기본 간격을 전역 축소하지 않는다.
- 본문폭452pt 이상은 2열, 그 미만은 1열. `원본 / 키 유형`, `새 값 전체 폭`, `사용처 / 전달 방식`, `필수 키` 순으로 정리한다. 기존 키 이름은 프로젝트·환경과 함께 헤더에 표시한다.
- “비우면 기존 값 유지”는 placeholder로 제공하고 반복 설명 행을 줄인다. PEM은 여러 줄 비밀 입력을 유지한다.
- 페이지 공용 오류와 편집 요청 오류의 소유권을 분리한다. 새 양식·다른 키에 이전 오류가 표시되지 않는 회귀를 추가한다. 실제 revision 충돌은 CAS를 유지하고 명시적인 최신 상태 다시 불러오기로 복구한다.
- `begin_add(scope,prefill)`, `show_dialog(ctx,snapshot,catalog)`, `has_dialog_or_pending_open()`에 해당하는 분리 계약을 구현해 PR3가 폼을 복제하지 않게 한다. snapshot 준비 후 prefill을 한 번만 소비한다.
- 공용 HTML의 `project_key_edit` 사례와 디자인 문서를 같은 치수로 갱신한다.

**완료 기준:** 1280×800pt에서 추가/편집·token/PEM·일반 오류 상태를 포함해 폼 전체와 footer가 보인다. 1280×720pt에서 일반 token/PEM 편집에 본문 스크롤이 없다. 예상 높이는 편집 약520/564pt, 신규 약590/634pt이며 실제 렌더 측정으로 확정한다. 작은 화면·긴 번역은 본문 스크롤을 유지해 필드를 자르지 않고 footer는 항상 보인다.

## PR3 — 우클릭 API 바로 등록

**대상 파일:** `crates/app/src/ui/{workspace.rs,notes.rs,file_tree.rs,environment.rs,credentials.rs}`,
`crates/app/src/app.rs`, 새 host `crates/app/src/app/project_api_quick_add.rs`, 해당 notes/environment/UI 및 App 회귀.

- 메모·터미널에서 우클릭 후 설정 페이지가 열리는 현재 동선/메모 진입 부재를 사용자 제스처 테스트로 고정한다.
- `API 바로 등록…`과 선택 항목의 `API 이름으로 등록`/`API 값으로 등록` 요청을 추가한다. 선택이 없으면 빈 폼을 열고, 선택 이름과 값은 서로 다른 필드에 넣는다.
- 메모 `NotesAction`→`SidebarAction`→App, 터미널 요청→App을 연결한다. 원본 workspace/runtime/session/environment를 클릭 시 캡처한다. 현재 활성 프로젝트로 다시 추측하지 않는다.
- App이 대기·로딩부터 모달 입력 fence를 세우고, 설정이 닫혀 있어도 직접 등록 창을 한 번만 렌더한다. 설정 내부 편집기와 전역 host가 같은 프레임에 중복 표시되지 않게 한다.
- API 서비스 이름(자유형)과 실행 변수명(환경변수 형식)을 별도 필드로 둔다. 선택한 API 이름은 서비스 이름에, 선택한 값은 마스크 입력에 넣는다. 에이전트/빌드 사용을 선택하면 실행 변수명이 필요하다. 두 이름을 같은 문자열로 강제하거나 API prefill을 단순 Variable prefill로 치환하지 않는다.
- 새 빠른 등록은 기존 ProjectKeysService의 단일 secret 저장과 프로젝트·환경 key identity를 사용하고, 서비스 이름/설명은 PR4 metadata로 연결한다. 기존 legacy 자유형 API와 유효한 프로젝트 바인딩은 보존·재사용한다. 별도 비밀값 복제 저장소를 만들지 않고, 바인딩/grant까지 성공한 항목만 에이전트 사용 가능으로 표시한다. 과거 바인딩 없는 개인 API를 임의 프로젝트에 결합하지 않는다.
- 저장 결과는 원래 프로젝트에만 반영하고 원래 메모/터미널 화면을 유지한다. Esc/취소/로딩 중 프로젝트 전환은 초안·undo·비밀 위젯 상태를 정리한다. 이전 ACK가 새 창을 닫지 못하게 한다.
- PR4 정책에 따른 `이 프로젝트 에이전트에서 사용` 선택을 제공한다. 기존 Build/StoreOnly 키의 사용처를 자동 변경하지 않는다.

**완료 기준:** 설정창 이동 없이 등록→수정→검색 반영이 연결된다. active/attached 프로젝트, 다른 runtime의 같은 session 번호, 로딩 중 Esc, 저장 실패, 프로젝트 전환을 검사한다. 모달을 여닫는 입력이 배경 PTY·메모 편집으로 전달되지 않는다.

## PR4 — 값 없는 API 검색과 사용 허용 계약

**대상 파일:** 신규 `crates/core/src/project_api_access.rs`, `crates/storage/src/project_api_access.rs`,
`crates/app/src/project_keys/catalog.rs`, 기존 `core/project_keys.rs`, `storage/project_keys.rs`,
`project_keys/{contracts.rs,service.rs,cli.rs,worker.rs}`, 모듈 등록·migration·테스트.

- 현재 project/environment에 한정한 catalog DTO를 정의한다. 목록은 key identity/name/kind/usage/delivery/revision/state/available/use_method와 선택적 서비스 이름/설명을 제공한다. 값·digest·credential 참조·PEM 경로·다른 프로젝트 목록은 제공하지 않는다.
- `env list --workspace ID --environment development|production --json`을 GUI 초기화 전에 처리한다. UI 전체 snapshot을 그대로 직렬화하지 않는다.
- 값 없이 조회하고 secret-store 호출이 0회인 테스트, development/production 및 다른 프로젝트 격리, locked/conflict/missing/GitHub-only 상태 구분을 추가한다.
- `(workspace,environment,key_id,principal/session,operation)`에 결합되는 추가형 grant를 저장한다. 기존 `KeyUsage::{Build,StoreOnly,LegacyAgent}`는 유지한다. 권한 없는 항목은 자동 사용 대상으로 광고하지 않는다.
- 키 이름만으로 API 주소/인증 방식을 추측하지 않는다. 선택적인 서비스명·설명·문서 URL은 일반 metadata로 검증하고, 값은 별도 기존 저장소에서만 보관한다.
- 회전 시 같은 key identity의 허가는 유지할 수 있지만 새 호출마다 현재 revision/상태를 재검증한다. 삭제·연결 해제·환경/세션 세대 변경·connection revoke는 대기 요청을 거절한다.

**완료 기준:** 등록됨/발견 가능/사용 가능을 분리해 출력한다. 기존 migration·Build 불승격·StoreOnly 동작을 보존하며, 일반 등록 키 전체를 자동으로 노출하거나 주입하지 않는다.

## PR5 — 에이전트 자동 발견과 최신 키 사용

**대상 파일:** 신규 `crates/app/src/project_keys/agent_access.rs`, `crates/agent-mcp/src/project_api_tools.rs`,
기존 `project_keys/{cli.rs,build.rs,process.rs}`, `agent-mcp/src/{lib.rs,server.rs,protocol.rs}`,
provider별 launcher/bootstrap, `app.rs`의 launch/agent bridge 접합, 해당 테스트.

- 세션의 workspace/environment와 접근 가능한 API 검색 방법을 모델이 실제 읽는 bootstrap 또는 scoped MCP tool로 전달한다. 환경변수에 문자열을 넣는 것만으로 자동 발견 완료라고 판단하지 않는다.
- 일반 에이전트 세션의 API scope를 발급한다. MAIN 전용 도구/권한을 일반 세션에 부여하는 방식은 사용하지 않는다. caller가 임의 workspace를 입력해 범위를 바꾸지 못하게 한다.
- 값 없는 `list_project_apis`와 허가된 recipe/키 조합을 사용하는 실행 경로를 제공한다. 로컬 CLI는 명시 scope의 기존 env-run과 명시 key 선택을 이용한다. cloud는 현재 공유 세션의 별도 key-use grant와 허가된 recipe로 제한한다.
- 기존 실행기에서 `LegacyBuildSet`과 `ExplicitGrantedKeys` 선택을 분리한다. 단순히 기존 `run_build` 전체 키 집합을 호출해 필요 이상의 키가 들어가지 않게 한다.
- 실행 직전 최신 원본·revision을 검증하고 선택된 키만 child env/제한 권한 임시파일에 제공한다. 키 평문을 모델 답변·tool JSON·명령 인수에 반환하는 도구는 만들지 않는다.
- 사용 후에는 키 identity/버전/시각/결과 receipt를 남긴다. 회전 후 새 호출은 최신 값, 이미 실행 중인 프로세스는 기존 snapshot 유지. 실패/취소/종료 시 임시파일을 정리한다.
- 실행 결과가 불명확하면 원래 operation을 조회하며 새 operation으로 자동 재실행하지 않는다. 일반 cloud 화면/입력 공유만으로 key-use 허가가 생기지 않는 테스트를 둔다.
- Claude/Codex/Grok/Kimi/Cursor의 실제 launch adapter별로 context/tool 설정이 도달하는지 검사한다. 현재 MAIN 자동 MCP가 Codex에 있다는 사실을 전체 provider 지원 근거로 쓰지 않는다. 검증하지 못한 adapter를 지원 완료로 표기하지 않는다.

**완료 기준:** 키 등록→프로젝트 scope의 발견→허용된 작업 실행→회전 후 다음 작업 최신 값 사용이 연결된다. 다른 프로젝트/환경/connection/generation은 거절된다. 기존 프로세스 환경을 임의로 바꿨다고 주장하지 않는다.

## PR6 — 통합 회귀와 최종 검토

- PR별 변경과 제품 요구사항의 1:1 체크표를 작성하고 root가 직접 diff를 검토한다.
- 두 프로젝트·동일 agent·동일 session 번호가 있는 별도 runtime·attached pane으로 등록/조회/실행을 검사한다.
- 분할 종료·모달 입력·터미널 scrollback·검색·메모 저장·agent 실행을 한 흐름으로 검사한다.
- 키 회전·원본 삭제·잠김·동시 수정·저장 실패·오래된 ACK·connection revoke를 검증한다. 실제 계정 값을 fixture로 복사하지 않는다.
- SSH는 동일 terminal reflow와 geometry 복원을 확인한다. 현재 remote helper가 Mac credential을 해석하지 않는 경계를 보존한다. 원격 프로젝트의 Mac 키가 사용 가능한 것처럼 광고하지 않는다. 서버 측 키 관리/credential 전송은 별도 계약 없이는 확장하지 않는다.
- 실제 desktop 폼 치수와 HTML 사례를 대조하고 두 언어 이상 긴 레이블 및 390×600pt fallback을 확인한다.
- 영향을 받은 전체 테스트, strict Clippy, fmt, boundary, diff, 기존 usage source guard를 실행하고 결과/소스 proof를 handoff에 기록한다.

## 검증 명령

아래는 최초 계획에 적었던 재검증 명령이다. 실제 실행한 명령·범위·결과는 최종 보고서에 기록했으며, 아래 목록 전체를 실행했다고 주장하지 않는다.
Cargo는 root가 아래 공통 환경으로 하나씩 실행한다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-integration-20261023
export CARGO_BUILD_JOBS=2
export CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target
python3 scripts/verify-provider-usage.py --check-only
cargo test --offline --locked -p session --test terminal_table_resize -- --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo table_resize_ -- --test-threads=4
cargo test --offline --locked -p terminal --lib streaming_resize -- --test-threads=4
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo ui::project_keys::tests -- --test-threads=4
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo project_keys -- --test-threads=4
cargo test --offline --locked -p storage project_key -- --test-threads=4
cargo test --offline --locked -p runtime project_keys -- --test-threads=4
cargo test --offline --locked -p agent-mcp -- --test-threads=4
cargo test --offline --locked -p deppy-sijo -p mcp-proxy -p terminal -p session -p runtime -p storage -p agent-mcp -p connector-ui -- --test-threads=4
cargo clippy --offline --locked -p deppy-sijo -p terminal -p session -p runtime -p storage -p agent-mcp -p connector-ui --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run --offline --locked -p xtask -- check-boundary
python3 scripts/verify-provider-usage.py --check-only
git diff --check
```

각 PR은 RED→최소 수정→GREEN→root 리뷰→해당 PR 변경만 커밋하는 단위로 구성한다.
이번 구현에서는 실제 커밋·푸시·PR 생성·빌드 배포·앱 재시작을 수행하지 않았다.
