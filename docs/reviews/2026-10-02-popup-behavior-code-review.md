# 팝업 동작 코드 리뷰 — 2026-10-02

후속 상태: 아래 0.5.3 리뷰의 3건은 **0.5.4에서 수정 완료**했다. [수정·최종 검증 보고서](2026-10-02-popup-behavior-fixes.md). 아래 내용은 수정 전 원인과 증거를 보존한 기록이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `crates/connector-ui/src/lib.rs:794` | 다음 MCP 승인 요청이 이전 허용 버튼 포커스를 계승 | 다음 Enter가 다른 요청의 허용 의도를 생성 | operation별 버튼 ID와 포커스 초기화 |
| medium | `crates/app/src/app.rs:22493` | 영구 삭제 확인 대기 상태가 전역 입력 차단에서 빠짐 | 첫 표시 프레임에 배경 단축키 실행·확인 상태 유실 가능 | FileTree 대기 확인을 렌더 전 차단에 포함 |
| medium | `crates/app/src/ui/file_tree.rs:2005` | 인라인 이름 확정 시 native 붙여넣기 신호가 남음 | 이름 저장과 터미널 붙여넣기 요청이 함께 발생 | 확정 프레임의 native 입력 소유권 유지 |

리뷰 당시 확인된 미해결 문제는 **3건**이었다. 당시 제품 코드는 수정하지 않았다. 기존 테스트 통과와 별개로, 현재 코드에 격리된 회귀 검증을 추가해 세 건 모두 실패를 재현했다.

## 검토 범위

- 현재 공용 팝업 shell/fields/footer/confirmation/input과 호출자 상태·비동기 결과 처리.
- 시안 사례 01/02/03/07/08/10/11/12/13/26–29: 워크스페이스 추가, 파일·폴더 생성, 세션·워크스페이스·프로세스 종료, 영구 삭제, 문서 확인, 이동한 폴더 확인.
- 공용 팝업으로 아직 옮기지 않은 관련 입력 양식과 Connector MCP 승인·신뢰·OAuth 호출 경로. 사례 04는 사용자 요청에 따라 인라인 편집이므로 터미널 입력과의 경계도 검토했다.
- 대상 바뀜, 확인·취소·Esc·닫기, 첫 프레임 입력 차단, 초안과 오류 후 재시도, 작업 ID와 실제 대상의 연결을 중점적으로 확인했다.
- `workstep`의 소스 코드 CLI 리뷰와 `systematic-debugging`의 원인 조사·최소 재현 절차를 적용했다. 이번 요청은 리뷰이므로 자동 수정·커밋 절차는 진행하지 않았다.

## Details

### 1. 다음 MCP 승인 요청이 이전 허용 버튼 포커스를 계승 — high

**위치:** `crates/connector-ui/src/lib.rs:794–823`, 큐 교체 경로 `crates/connector-service/src/coordinator.rs:3652–3680`.

승인창은 항상 `connector_approval` ID를 쓰고 버튼 ID도 요청의 `operation_id`로 구분하지 않는다. `remove_pending_invocation`은 기존 요청을 제거한 뒤 `refresh_approval_snapshot`에서 다음 요청을 바로 노출한다. 두 요청 사이에 빈 프레임이 생긴다는 보장이 없다.

실제 공개 `ConnectorUi::render`를 사용한 재현:

1. 첫 요청의 “한 번 허용” 버튼에 키보드 포커스를 놓고 Enter로 승인 의도를 생성한다.
2. snapshot을 다른 operation/tool의 두 번째 요청으로 교체한다.
3. 새 요청의 버튼을 선택하지 않고 Enter만 누른다.
4. `ResolveApproval { operation_id: second, decision: AllowOnce }`가 생성된다.

실측 출력: `next approval after bare Enter: operation=second decision=AllowOnce`.

다음 요청에 사용자가 새로 포커스를 옮기지 않아도 허용 버튼이 활성화된다. 승인 대상별 동작 ID와 포커스 초기화가 필요하다. 창 Area까지 매 요청마다 새로 만들기보다는 기존 공용 확인창의 고정 Area/대상별 동작 분리 방식을 참고할 수 있다.

**검증 한계:** 실제 서버 도구는 실행하지 않았다. 실제 UI가 내보내는 허용 intent와 실제 서비스의 연속 큐 경로를 확인했다. 최신 탭 선 수정에서 새로 생긴 회귀는 아니며 현재 남아 있는 Connector 승인 결함이다.

### 2. 영구 삭제 확인 첫 프레임에 전역 단축키가 통과 — medium

**위치:** `crates/app/src/app.rs:22493–22505`, `29713`, `29778–29779`; `crates/app/src/ui/file_tree.rs:1355`, `3341`.

휴지통 작업이 `TrashUnavailable`로 끝나면 `complete_io`가 `FileTreeUi.confirm_delete`를 채운다. App은 worker 결과를 받은 후 전역 단축키를 처리하지만, `background_modal_pending`은 FileTree의 확인 상태를 포함하지 않는다. 실제 `permanent_delete_confirm`이 나중에 그려지면서 차단을 설정하므로 첫 표시 pass에는 늦다.

재현은 실제 FileTree 휴지통 요청·완료와 실제 configured shortcut 소비 함수를 사용했다. 관련 없는 다른 팝업은 없는 조건으로 App의 차단 predicate 결과를 반영한 뒤, 첫 삭제 확인을 그리기 전에 Cmd+W를 처리했다.

실측 출력: `global shortcut admitted before first trash modal: Some(ClosePane)`.

또한 소스상 같은 시점의 Cmd+B는 `ToggleSidebar`를 처리해 `self.file_tree`를 `None`으로 바꾼다(`app.rs:22538`). 이 경로에서는 그 안의 삭제 확인·재시도 상태까지 사라진다. FileTree의 대기 확인 상태를 렌더 전 입력 차단에 포함해야 한다.

**검증 한계:** 전체 App/native GUI를 기동한 E2E 재현은 아니다. 실제 구성 요소로 단축키 허용을 관측하고 App 처리 순서·호출 경로를 검토했다. PTY 종료나 파일 영구 삭제를 실행하지 않았다. 일반 터미널 문자 입력 누출까지 확인한 것으로 해석하면 안 된다.

### 3. 이름 저장과 터미널 native 붙여넣기가 함께 발생 — medium

**위치:** `crates/app/src/ui/file_tree.rs:2005–2008`, `6868–6881`; native batch 소비 `crates/app/src/ui/workspace.rs:5423–5428`, `7869–7870`, `7905`, `8098–8106`.

인라인 이름 편집은 확정 시 egui의 Text/Paste/Key 이벤트를 제거하고 편집 포커스를 해제한다. macOS의 native key monitor가 따로 보관한 `clipboard_paste` 신호는 제거하지 않는다. 같은 프레임에 뒤에서 실행하는 WorkspaceUi는 이제 TextEdit 포커스가 없는 상태로 그 신호를 소비해 터미널 붙여넣기를 요청한다.

실제 FileTree panel → 클립보드 소비 플래그 전달 → `WorkspaceUi::show_with_input` 순서를 하나의 harness에서 실행했다. native monitor의 실제 기록 함수에 테스트 신호를 넣었고, egui Paste와 Enter를 한 프레임에 전달했다. 터미널 composer는 표시하지 않는 조건이다.

결과:

- `CommitWorkspaceName(ws-inline, Serenity-alias)`가 생성돼 이름 확정 경로가 실행됐다.
- 같은 프레임에 `ReadTerminalClipboard { operation: WorkspaceIoOperation(1), generation: 1 }`가 생성됐다.
- 터미널 요청이 없어야 한다는 회귀 assertion이 실패했다.

빠르게 붙여넣고 확정하는 프레임에서는 편집창의 native 입력 소유권을 확정 시점까지 유지해야 한다. 같은 인라인 도우미를 사용하는 세션 이름 변경에도 동일한 원인이 존재하므로 함께 수정·검증하는 편이 좋다.

**검증 한계:** OS 클립보드를 실제로 읽거나 사용자 PTY에 입력하지 않았다. native batch와 실제 terminal clipboard 요청의 연결을 검증했다. native macOS 입력을 손으로 재생한 테스트는 아니다.

## 실제 실행한 테스트와 증거

| 검증 | 결과 | 로그 |
| --- | --- | --- |
| 기존 App 전체 | 2514 passed / 0 failed / 28 ignored, 47.74s | `/tmp/deppy-popup-behavior-app-tests-20261002.log` |
| 기존 Connector UI 전체 | 17 passed / 0 failed, 0.89s | `/tmp/deppy-popup-behavior-connector-tests-20261002.log` |
| 독립 Codex CLI 소스 리뷰 | exit 0; 발견 2건은 위 2·3번과 중복 | `/tmp/deppy-popup-behavior-user-review-cli-20261002.log` |
| 영구 삭제 첫 프레임 회귀 probe | 1 failed, 0.02s; 문제 재현 | `/tmp/deppy-popup-behavior-delete-probe-20261002.log` |
| 다음 MCP 승인 포커스 회귀 probe | 1 failed, 0.06s; 문제 재현 | `/tmp/deppy-popup-behavior-approval-probe-20261002.log` |
| 이름 변경 native paste 회귀 probe | 1 failed, 0.06s; 문제 재현 | `/tmp/deppy-popup-behavior-rename-probe-20261002.log` |
| 제품 코드 hash | 기존 0.5.3 source hash와 동일 | `/tmp/deppy-popup-behavior-review-integrity-20261002.json` |

격리 checkout: `/private/tmp/deppy-popup-behavior-review-20261002`. HEAD에 현재 제품의 tracked dirty diff와 untracked source를 그대로 적용하고 테스트 코드만 추가했다. 제품 worktree에는 probe를 넣지 않았다. 기존 테스트의 성공과 새 회귀 검증의 의도한 실패를 구분한다.

```sh
# 제품 worktree에서 실행한 기존 테스트
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1
cargo test --offline --locked -q -p connector-ui -- --test-threads=1

# 격리 checkout에서 각 문제를 재현 (현재 코드에서는 각각 exit 101)
cd /private/tmp/deppy-popup-behavior-review-20261002
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_behavior_probe_trash_failure_before_shortcuts -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_behavior_probe_next_connector -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_behavior_probe_workspace_rename -- --nocapture --test-threads=1
```

초기 fixture에서 egui Preedit의 구형 tuple 형태·잘못된 range 형태를 사용해 컴파일 오류가 났고, 현재 API에 맞춰 고친 후 위 검증을 실행했다. 제품 컴파일 실패가 아니다.

### 발견 사항에서 제외한 가설

새 폴더 양식에 synthetic Preedit+Enter만 전달하면 생성 요청이 나온다. 하지만 vendored macOS winit의 `view.rs:440–497`는 IME가 소비한 키를 일반 KeyInput으로 전달하지 않는다. 현실적인 OS 경로에서 발생한다고 확인하지 못했으므로 버그로 집계하지 않았다. 해당 초기 실험은 `/tmp/deppy-popup-behavior-probes-20261002.log`에만 남겼다.

## 작업 상태와 남은 일

리뷰 기준은 performance worktree `fix/cloud-agent-ended-sessions`, base `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, 제품 0.5.3이다. 제품 source hash는 `caad16d87d931151687d5bf6972f043e4e72e28c36a6f631dfad436733074557`로 그대로다.

이번 작업에서 수정한 파일은 이 보고서와 `docs/CODEX_HANDOFF.md`뿐이다. 제품 수정·릴리스 빌드·버전 변경·커밋·푸시·앱 종료·재실행은 없었다. 0.5.3 PID21631의 기존 실행 파일 경로와 실행 유지 상태를 확인했다.

**남은 구현:** 위 3건 수정과 정식 회귀 테스트 추가. 실제 native 입력·파일 선택창·파일 삭제·프로세스 종료 E2E, RSS/GPU 실측, Clippy 또는 전체 popup 시각 회귀는 이번 리뷰에서 실행하지 않았다.
