# 팝업·우클릭·폴더·파일트리 병렬 코드 리뷰 — 2026-10-14

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| high | app.rs:15397 / file_tree.rs:4564 | R1 영구삭제 확인 후 대상 교체를 허용 | 새 파일·루트 밖 파일 영구삭제 | root·원본 객체 identity 재검증 |
| high | file_tree.rs:3395 | R2 Rename Enter/Escape가 터미널로 전달 | 파일 편집 키가 세션 입력으로 처리 | 종료 frame 입력 소비·소유권 보호 |
| medium | connector-ui/lib.rs:1072 | R3 취소한 OAuth 키가 undo로 복원 | 다른 연결 설정에 이전 키 제출 | 비밀값 widget undo 기록 제거 |
| medium | renderer_egui.rs:599 | R4 터미널이 파일트리 focus를 회수 | 파일 삭제·이동 단축키 동작 불가 | 명시적인 tree keyboard owner 유지 |
| medium | file_tree.rs:3385 | R5 Rename widget ID가 행 위치에 의존 | 목록 갱신 중 입력 focus 소실 | path·edit generation으로 ID 고정 |
| medium | file_tree.rs:5921 | R6 오른쪽 버튼으로 workspace drag 시작 | 우클릭 이동 중 목록 자동스크롤 | Primary 버튼만 drag 시작·진행 |
| medium | file_tree.rs:2000 | R7 권한 복구 후 폴더 재조회 불가 | Refresh 후에도 폴더 비활성화 | 오류 경로의 명시적 retry 허용 |
| medium | app.rs:14398 | R8 symlink root 변경 이벤트 누락 | 외부 파일 변경이 목록에 미반영 | physical→logical 경로 변환 |
| medium | app.rs:5372 | R9 사라진 picker 경로를 성공 처리 | 기존 workspace anchor 제거 | probe 실패 시 DB 변경 거부 |
| low | file_tree.rs:1442 | R10 부분 복사 오류 시 목록 갱신 누락 | watcher 장애 시 복사 결과 미표시 | 실패 후 영향 디렉터리 재조회 |

GPT-6.1-Sol / xhigh 서브에이전트 3명이 팝업, 메뉴·트리 UI, 폴더·파일 IO를 나눠 검토했다. Root가 생산 호출부와 재현 로그를 다시 대조했고, 독립 probe 6개를 재실행했다. 확정 결함은 중복 제거 후 **10건: high 2 / medium 7 / low 1**이다. R1의 부모 symlink 변경과 같은 이름의 inode 교체는 동일한 확인 대상 검증 누락이므로 한 항목으로 합쳤다.

검토 대상은 최신 canonical checkout `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`와 검토 시작 시점의 미커밋 wide-cursor 수정이다. 기존 `/Users/jr/Desktop/projects/deppy-sijo`의 dirty checkout과 기존 작업을 보존했다. 제품 수정·커밋·푸시·사용자용 배포·앱 재실행은 수행하지 않았다. 검사용 코드만 별도 임시 worktree에 추가했고, 테스트 버전은 0.12.1이다.

## Details

### R1 — 영구삭제 확인 대상이 바뀌어도 삭제 실행

휴지통 작업이 실패해 영구삭제 확인창이 열린 뒤 원래 파일을 이동하고 동일 이름·종류의 새 파일을 만들면, `invalidate_stale_delete_confirm`은 이름·종류만 비교해 확인창을 유지한다. 부모 디렉터리를 외부 디렉터리로 향하는 symlink로 교체한 경우에는 root Refresh 후에도 확인창이 유지된다. 확인 요청은 `DeletePermanently { target }`만 보내고, worker는 root·원본 객체 identity 검증 없이 그 경로를 삭제한다.

- 위치: `crates/app/src/ui/file_tree.rs:1801`, `:4564`; `crates/app/src/app.rs:15397`.
- 재현: 실제 확인창 클릭과 생산 native worker를 연결한 격리 테스트 2개. 새 inode 파일이 삭제되거나, symlink가 가리키는 루트 밖 파일이 삭제됐고 원본 파일은 남았다. 휴지통 실패는 `TrashUnavailable` completion으로 주입했으며, 영구삭제 자체는 생산 worker가 실제 임시 파일에 실행했다.
- 권고: 확인 상태와 IO 요청에 원본 root·객체 identity를 보존하고, 확인 승인과 worker 삭제 시점 모두 대상 교체·root 이탈을 거부한다. UI에서만 확인창을 닫으면 승인 이후 변경 경쟁이 남으므로 실제 삭제 경계도 검증해야 한다.
- 근거: [folders-scratch-red.log](evidence/2026-10-14-popup-context-folder-file-tree/folders-scratch-red.log), [folders-probes.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch), [folders-escape.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-escape.patch).

### R2 — 파일 Rename 종료키가 현재 터미널 입력으로 처리

파일 행의 singleline TextEdit는 Enter/Escape에서 focus를 잃는다. Rename 코드는 `key_pressed`로 완료·취소만 판정하고 raw 이벤트를 소비하지 않는다. inline Rename은 App의 modal fence에도 포함되지 않는다. 같은 frame에서 Workspace가 더 이상 TextEdit focus를 보지 않고 terminal 입력을 허용해, 생산 input mapper가 Enter를 `[13]`, Escape를 `[27]`로 변환한다.

- 위치: `crates/app/src/ui/file_tree.rs:3395`; App `background_modal_pending:25528`, terminal owner `:36233`; Workspace keyboard gate `crates/app/src/ui/workspace.rs:10570`; renderer `crates/terminal/src/renderer_egui.rs:599`.
- 재현: 실제 egui 0.36.1의 Rename 분기·Workspace gate·renderer focus 흐름과 생산 `input_mapper.rs`를 조합한 source-derived probe. Enter와 Escape 둘 다 종료 후 터미널 전달 바이트가 생성됐다. Root 재실행도 exit101이었다. 실행 중인 전체 App GUI에서 실제 세션에 입력을 보내는 검사는 하지 않았다.
- 권고: 공통 inline editor의 입력 소유권 처리를 재사용해 종료키 및 대응 native 입력 batch를 소비하고, 편집이 끝나는 frame까지 terminal 입력을 차단한다. sidebar와 terminal을 함께 렌더링하는 회귀로 IME·Enter·Escape를 검증한다.
- 근거: [file-rename.rs](evidence/2026-10-14-popup-context-folder-file-tree/file-rename.rs), [file-rename.log](evidence/2026-10-14-popup-context-folder-file-tree/file-rename.log).

### R3 — 취소한 OAuth client secret이 다음 연결에 복원

`password(true)`는 표시를 마스킹하지만 egui TextEdit의 undo state에는 원래 값이 남는다. `clear_sensitive_drafts`, draft 교체와 Drop은 소유한 문자열만 지운다. 첫 OAuth 작업 취소 후 빈 snapshot을 거쳐 다른 server/operation의 client 입력을 열어 Cmd+Z를 누르면 이전 키가 복원된다.

- 위치: `crates/connector-ui/src/lib.rs:1072`; cleanup `:114`, draft sync `:1151`, draft Drop `:1613`.
- 재현: 생산 `ConnectorUi::render`와 egui_kittest를 사용했다. 실제 Cancel intent → cleanup → 빈 snapshot → 두 번째 operation → Cmd+Z → Save intent 순서에서 이전의 가짜 18-byte 키가 두 번째 `SubmitOAuthClient`에 정확히 포함됐다. 실제 provider나 실제 비밀값은 사용하지 않았다. Root 재실행 exit101.
- 권고: tracked widget ID와 egui context를 이용해 공유 undo 기록까지 지우거나, 기존 masked secret buffer/비밀값 TextEdit 패턴을 적용한다. cancel·성공·server/operation 변경·화면 숨김 경계를 검증한다.
- 근거: [connector-secret-undo.rs](evidence/2026-10-14-popup-context-folder-file-tree/connector-secret-undo.rs), [connector-secret-undo-red.log](evidence/2026-10-14-popup-context-folder-file-tree/connector-secret-undo-red.log), [실제 rustc 명령](evidence/2026-10-14-popup-context-folder-file-tree/connector-secret-undo-build-command.json).

### R4 — 파일트리 키보드 focus가 같은 frame에 회수

파일 클릭은 `tree_keyboard_focus_id()`에 focus를 요청하지만 terminal gate는 TextEdit·popup·modal만 제외한다. renderer는 활성 terminal이 IME owner가 아니면 즉시 focus를 요청한다. 다음 frame에 CmdDelete·CmdOptionV가 요구하는 tree focus가 없어 파일 단축키가 무시된다.

- 위치: `crates/terminal/src/renderer_egui.rs:599`; 파일 클릭 `file_tree.rs:3904`, 단축키 `:4379`, `:4423`; Workspace gate `:10570`.
- 재현: 실제 egui와 생산 helper/gate/renderer 조건을 사용한 source-derived probe에서 파일 클릭 직후 tree focus=true, terminal 렌더 후=false, 다음 frame=false를 확인했다. Root 재실행 exit101. leaf-only 트리 검사는 이 조합을 다루지 않는다.
- 권고: 파일트리도 명시적 keyboard owner로 취급하고 terminal 클릭 전에는 focus를 가져오지 않는다. 논리적 owner와 native IME owner를 함께 관리한다.
- 근거: [tree-focus.rs](evidence/2026-10-14-popup-context-folder-file-tree/tree-focus.rs), [tree-focus.log](evidence/2026-10-14-popup-context-folder-file-tree/tree-focus.log).

### R5 — 목록 갱신이 Rename 편집 focus를 잃게 함

Rename TextEdit에 안정적인 ID가 없고 앞선 행의 allocation에 따라 auto ID가 바뀐다. watcher의 listing 적용은 동일한 편집 대상을 유지하면서 flat 목록을 재구성할 수 있다. 앞에 형제 파일이 추가되면 현재 보이는 편집창의 ID가 바뀌어 후속 타이핑이 무시된다.

- 위치: `crates/app/src/ui/file_tree.rs:3385`, listing 적용 `:1776`.
- 재현: 실제 ScrollArea::show_rows와 Rename allocation을 사용한 source-derived probe. b.txt 편집 중 a.txt 삽입 → widget ID 변경 → focus=false → 입력 x 미반영. Root 재실행 exit101.
- 권고: full path·edit generation ID를 지정하고 같은 대상의 재정렬은 focus·caret를 유지한다. 대상이 사라질 때는 편집 상태를 무효화한다. R2의 종료키 소비와는 별도 문제다.
- 근거: [file-rename-row-id.rs](evidence/2026-10-14-popup-context-folder-file-tree/file-rename-row-id.rs), [file-rename-row-id.log](evidence/2026-10-14-popup-context-folder-file-tree/file-rename-row-id.log).

### R6 — 오른쪽 버튼 이동이 workspace 자동스크롤 시작

workspace reorder 상태는 generic `drag_started()`로 시작한다. 마지막 drop만 Primary로 제한되어, 오른쪽 버튼을 누른 채 목록 가장자리로 이동하면 실제 순서 변경 없이도 자동스크롤이 실행된다.

- 위치: `crates/app/src/ui/file_tree.rs:5921`; 자동스크롤 `:2790`.
- 재현: 생산 drag/autoscroll 함수를 적용한 실제 egui probe에서 Secondary만 누른 상태로 4 frame에 23.75pt 스크롤했다. Root 재실행 exit101. 잘못된 reorder까지 발생했다고 주장하지 않는다.
- 권고: drag 시작·진행·자동스크롤 소유권을 모두 Primary로 제한한다. 정상 Primary edge-scroll과 Secondary 음성 회귀를 함께 둔다.
- 근거: [workspace-secondary-drag.rs](evidence/2026-10-14-popup-context-folder-file-tree/workspace-secondary-drag.rs), [workspace-secondary-drag.log](evidence/2026-10-14-popup-context-folder-file-tree/workspace-secondary-drag.log).

### R7 — 폴더 접근 권한 복구 후 재시도 불가

자식 폴더 PermissionDenied는 inaccessible 표시를 남기고 접는다. Refresh는 root와 펼친 폴더를 조회하며, `toggle_dir`는 inaccessible이면 즉시 반환한다. 성공 listing은 정확히 그 경로를 재조회해야 표시를 지우므로, 사용자 Refresh·펼치기만으로 복구할 수 없다.

- 위치: `crates/app/src/ui/file_tree.rs:1855`, `:1966`, `:2000`.
- 재현: 임시 폴더 mode 000으로 실제 PermissionDenied 확인 → 0700 복원·read_dir 성공 → Refresh·펼치기에서 `retry=false, inaccessible=true`. scratch 행동 assertion exit101.
- 권고: 명시적 Refresh/행 retry에서 오류 경로를 다시 조회한다. 자동 무한 재시도와 구분해 사용자의 복구 의도를 처리한다.
- 근거: [folders-scratch-red.log](evidence/2026-10-14-popup-context-folder-file-tree/folders-scratch-red.log), [folders-probes.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch).

### R8 — symlink root의 macOS 파일 변경 감지 누락

notify FSEvents는 watch 경로를 canonicalize하고 physical 경로의 callback을 전달한다. App watcher는 logical alias root prefix를 요구해 이 이벤트를 필터에서 버린다.

- 위치: `crates/app/src/app.rs:14398`; root 적용 `:14369`; pinned notify 8.2.0 fsevent canonical watch/callback 연결부도 확인했다.
- 재현: 실제 `AppFileTreeWatcher`로 symlink alias를 watch하고 physical target 안에 파일 생성. native callback의 `physical/new.txt` 수신을 확인한 후에도 `refresh_queued=false`. scratch 행동 assertion exit101. callback 미수신 timeout으로 추정한 결과가 아니다.
- 권고: canonical watch root와 UI logical root의 경로를 변환하고 ignore/hidden/env 필터도 동일한 namespace로 적용한다. 일반 파일 변경은 실제 검증했고, 같은 필터의 .env 감지 영향은 소스 연결에 따른 추론이다.
- 근거: [folders-scratch-red.log](evidence/2026-10-14-popup-context-folder-file-tree/folders-scratch-red.log), [folders-probes.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch).

### R9 — picker 선택 이후 사라진 폴더가 기존 anchor 제거

SetProjectPath는 non-empty 경로의 folder identity probe 실패를 None으로 바꾼 후 그대로 DB에 저장한다. 의도적인 빈 경로 연결 해제와 잘못된 picker 결과를 구분하지 않는다.

- 위치: `crates/app/src/app.rs:5372`.
- 재현: 격리 SQLite에 기존 path·dev/ino·volume 저장 → 선택 폴더 이동 → 실제 settings worker 실행. `ProjectPathSet(Ok)`, 이전 anchor가 None으로 변경됐다. scratch 행동 assertion exit101.
- 권고: non-empty 선택 결과는 유효한 디렉터리 identity를 얻은 후에만 저장하고, 실패하면 기존 path·anchor·volume을 보존한다. 빈 경로의 명시적 detach는 유지한다.
- 근거: [folders-scratch-red.log](evidence/2026-10-14-popup-context-folder-file-tree/folders-scratch-red.log), [folders-probes.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch).

### R10 — 부분 복사 실패 시 실제 게시 결과 미표시

생산 복사 worker는 항목들을 순차 게시한다. 첫 항목 복사 후 다음 항목이 실패할 수 있지만 IO Err completion은 `pending.refresh`를 실행하지 않는다. 정상 watcher가 보정할 수 있으므로 watcher 미동작 조건의 low 항목으로 제한한다.

- 위치: `crates/app/src/ui/file_tree.rs:1442`; 생산 worker `crates/app/src/app.rs:15251`.
- 재현: 임시 첫 파일은 복사 성공, 다음 파일은 실제 권한 오류 → worker Err. watcher 없이 완료·listing을 처리하자 `first_published=true, first_visible=false`. scratch 행동 assertion exit101.
- 권고: 부분 효과가 가능한 실패에서도 영향 디렉터리를 갱신하고 필요하면 실제 적용 결과를 표시한다. 전체 rollback을 이 리뷰의 요구사항으로 추가하지 않는다.
- 근거: [folders-scratch-red.log](evidence/2026-10-14-popup-context-folder-file-tree/folders-scratch-red.log), [folders-probes.patch](evidence/2026-10-14-popup-context-folder-file-tree/folders-probes.patch).

## 범위·검증

| Reviewer | 검토 범위 |
|---|---|
| popups | 공통 popup shell·window·modal/escape/fence, 문서 dirty/conflict/cap/moved 확인, session/workspace 닫기, 환경·프로젝트 키 dialogs, workspace add·clone/picker, launcher, resource/ports, fleet, prompt/diff/session manager/native settings, credential/connector |
| menus/tree UI | 공통 context menu와 호출부, file-tree workspace/live/inactive/header/file-row 메뉴, 선택·다중선택·접기·가상화·marquee·drag/drop·rename/delete, Workspace pane/empty/output/tab 메뉴, Git·Notes·Fleet·환경 경로 메뉴와 target routing |
| folders/files | picker 요청/완료 목적·대상 전달, workspace add/open/relink·volume 검증, clone staging/recovery, listing/watch generation, rename/move/copy/trash/permanent-delete worker |
| Root | App·Workspace·renderer 입력 소유권, 후보 중복·production 도달 경로, 실제 native filesystem/watcher·SQLite·ConnectorUi 및 source-derived egui 재현, 기존 관련 검사와 원본 보존 |

| 실제 실행 | 결과 |
|---|---|
| App `popup` | 58 pass / 10 기존 ignored / exit0 |
| App `context_menu` | 9 pass / 5 기존 ignored / exit0 |
| App `file_tree` | 195 pass / 3 기존 ignored / exit0 |
| App `workspace` | 578 pass / 4 기존 ignored / exit0 |
| App `picker` | 6 pass / 0 ignored / exit0 |
| `connector-ui` 전체 | 26 pass / doc0 / exit0 |
| 위 검사 중복 제거 | **고유 838 pass / 15 기존 ignored** |
| scratch 신규 filesystem/picker/watch 회귀 | compile 성공 / 0 pass · 6 예상 행동 fail / exit101 |
| 신규 standalone 메뉴 4개·OAuth 1개 | compile 성공 / 5 예상 행동 fail / Root 재실행 동일101 |
| 공통 context-menu Escape control | compile 성공 / pass / Root 재실행 exit0 |
| canonical 제품 diff 시작·종료 비교 | `cmp` exit0, 기존 변경만 보존 |

App 필터는 겹친다. connector 로그에는 이전 targeted 1개 실행도 포함되어 있으므로 test 이름을 crate와 함께 중복 제거했다. **전체 App suite나 실행 중인 앱 GUI E2E를 통과했다고 주장하지 않는다.** 메뉴 4개 probe는 생산 흐름을 옮긴 source-derived egui 재현이며, 실제 전체 App composite 회귀로 옮겨야 한다. Connector probe는 생산 `ConnectorUi`를 직접 실행했다. 신규 filesystem 검사는 생산 worker/실제 임시 FS/native watcher/격리 SQLite를 실행했다. 재현용 바이너리를 사용자용 앱으로 배포하지 않았다.

공통 context-menu Escape 전달 후보는 closing pass의 `ctx.any_popup_open()`이 유지되어 terminal gate가 닫히므로 제외했다. Remote Trust/OAuth consent focus 후보는 임의 UI snapshot에서만 재현했고 production 동시 작업 도달 경로를 입증하지 못해 확정 결함에서 제외했다. 도달하지 않는 legacy ApprovalsUi entry point도 결함으로 세지 않았다.

첫 scratch patch의 여분 closing brace는 컴파일 전에 수정하고 rustfmt parse·git apply check 후 사용했다. egui probe의 texture-delta cleanup, control의 popup 상태 precondition, connector dependency cohort 일시 누락도 고쳤다. 이러한 harness 오류는 제품 결함이나 행동 RED에 합산하지 않았다. 설치된 review skill의 필수 gstack checklist가 없어서 해당 workflow는 중단했으며, 직접 병렬 소스 검토와 실제 재현으로 이 보고서를 작성했다.

후속 수정은 R1 영구삭제 대상 보호, R2/R4 입력 소유권, R3 비밀값 undo 정리, R5/R6 행 ID·Primary drag, R7/R8/R10 listing/watch 회복, R9 picker 검증을 각각 독립 PR로 묶을 수 있다. 현재 요청 범위의 검토·근거 기록은 완료했고 위 결함은 아직 제품 코드에 수정하지 않았다.

전체 실행 명령·근거 SHA·재현 방법은 [evidence README](evidence/2026-10-14-popup-context-folder-file-tree/README.md)와 [manifest.json](evidence/2026-10-14-popup-context-folder-file-tree/manifest.json)에 보관했다.

## 후속 수정 — 2026-10-14

사용자 요청으로 R1–R10 모두 소스 수정·통합·검증을 완료했다. 원래 리뷰와 RED 근거는 이 문서에 보존하며, 현재 수정 결과·실제 검사·실행 범위는 [수정 QA](../qa/2026-10-14-popup-context-folder-fixes.md)를 참조한다. GPT-6.1-Sol/xhigh 별도 worktree3개와 Root 작업을 교차 리뷰했다. 고유3354 PASS/43기존ignored, strict/boundary 통과. 사용자 실행 앱 재실행·배포는 이번 task에서 수행하지 않았다.
