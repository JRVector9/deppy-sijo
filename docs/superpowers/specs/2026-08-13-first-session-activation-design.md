# First Session Activation Design

Date: 2026-08-13
Status: approved direction, awaiting written-spec review

## Objective

앱을 처음 실행한 뒤 저장된 세션을 선택했을 때 터미널 복원이 멈춰 보이는 문제를 제거하고, 선택한 터미널이 준비되는 첫 프레임부터 별도의 두 번째 클릭 없이 키 입력을 받게 한다.

기존 세션 영속성, 워크스페이스 전환, warm runtime 상한, `.env`/keychain 검증, archived agent의 열람 전용 정책은 유지한다.

## Confirmed failure chain

1. `App`은 `persisted_activity_panes`를 빈 맵으로 생성한다.
2. 시작 시 `refresh_workspaces()`는 AgentState worker에 Catalog projection만 예약하고 즉시 반환한다.
3. 그 직후 시작 복원 여부를 아직 빈 `persisted_activity_panes`로 검사하므로 저장된 pane이 있어도 `RestoreWorkspace`가 예약되지 않는다.
4. Catalog 결과가 나중에 도착해 sidebar에 저장 세션을 표시하지만, 현재 코드는 그 시점에 시작 복원을 다시 요청하지 않는다.
5. 저장 세션 행은 정확한 `pane`을 이미 갖고 있으면서도 클릭 시 `SwitchWorkspace(workspace_id)`만 반환한다.
6. 현재 활성 워크스페이스의 저장 세션이면 `switch_workspace()`가 동일 ID를 보고 즉시 반환하므로 복원 명령이 전혀 발생하지 않는다.
7. 다른 cold 워크스페이스이면 선택 pane 정보가 사라진 채 전체 `RestoreWorkspace`만 실행되어, 선택한 터미널과 native keyboard focus를 우선할 수 없다.

실제 번들 로그에서는 앱 시작이 `20:52:54Z`였지만 첫 pane 복원 이벤트가 사용자의 후속 선택 이후인 `20:53:58Z`부터 나타났다. 활성 저장 워크스페이스는 pane 1개이고 출력도 약 20 KiB뿐이어서 로그 크기나 pane 수가 원인이 아니다.

## Selected approach

### 1. Catalog-driven one-shot startup restore

AgentState Catalog가 `persisted_activity_panes`를 갱신한 직후, 정확한 active workspace/runtime lifetime에 저장 pane이 있으면 복원을 한 번 예약한다.

각 `WorkspaceRuntime`은 해당 lifetime에서 restore가 이미 예약됐는지를 기록한다. dotenv continuation admission에 성공한 뒤에만 예약 완료로 표시해, admission 실패를 성공으로 오인하지 않는다. 동일 Catalog 재발행이나 UI repaint는 중복 복원을 만들지 않는다.

시작 생성자에서 비어 있는 비동기 projection을 검사하는 현재 코드는 제거한다. SQLite를 UI thread에서 다시 읽는 동기 우회도 추가하지 않는다.

### 2. Persist exact pane activation intent

저장 세션 행 클릭은 `workspace_id + pane`을 보존하는 별도 typed action을 반환한다. App은 action을 실행하기 직전에 pane이 최신 `persisted_activity_panes` catalog에 여전히 존재하는지 확인해 stale row를 fail-closed 처리한다.

이미 materialize된 pane이면 기존 `FocusPane`을 사용한다. Cold/unmaterialized pane이면 exact pane restore intent로 전환한다.

### 3. Exact-pane-first cold restore

Cold workspace 전환은 선택 pane이 있으면 다음 순서를 사용한다.

1. 기존 dotenv worker에서 workspace/root/runtime freshness와 keychain을 검증한다.
2. `SetSessionDefaultEnv`와 terminal cache policy를 적용한다.
3. 기존 `RestoreWorkspacePane { pane }` 명령으로 선택 pane을 먼저 materialize한다.
4. 기존 `FocusPane { pane }` 명령으로 mux focus를 선택 pane에 고정한다.
5. 기존 `RestoreWorkspace`로 나머지 저장 pane을 이어서 복원한다.

새 RuntimeCommand나 wire protocol variant는 추가하지 않는다. Cross-workspace attached-pane durable barrier와도 섞지 않도록 primary activation 전용 dotenv continuation을 둔다.

각 runtime 명령이 별도 event를 발생시키므로 선택 pane의 Mux/Viewport는 나머지 pane 복원이 끝나기 전에 UI에 도달할 수 있다.

### 4. One-shot native terminal focus

저장 세션을 클릭하는 순간 WorkspaceUi에 정확한 pane의 pending terminal focus를 건다. 이는 연결 중 placeholder에서는 소비하지 않고, 해당 pane의 첫 실제 terminal viewport가 그려지는 프레임에 한 번만 `request_focus()`와 terminal focus-lock filter를 적용한다.

Runtime focus가 아직 이전 pane을 가리키는 동안에는 pending pane만 입력 소유자가 된다. 선택 pane이 준비되기 전 키 입력을 다른 세션으로 보내거나 임의 버퍼에 보관하지 않는다.

Full restore가 이미 진행 중일 때 사용자가 다른 저장 pane을 선택하는 경우를 위해 App은 `workspace_id + runtime_instance + pane` pending focus identity를 유지한다. 해당 exact pane이 mux에 나타났을 때 한 번 `FocusPane`을 보내고 stale runtime/workspace 전환 시 폐기한다.

## State and interfaces

- `WorkspaceRuntime`
  - 현재 runtime lifetime의 restore admission 여부를 보유한다.
- `SidebarAction` / `WorkspaceControllerAction`
  - persisted session activation을 `workspace_id + pane`으로 운반한다.
- `PendingDotenvContinuation`
  - cross-workspace attachment와 구분되는 primary exact-pane restore variant를 추가한다.
- `App`
  - Catalog 적용 후 active restore를 one-shot 보장한다.
  - cold switch에 optional preferred pane을 전달한다.
  - exact pending pane focus를 runtime lifetime으로 fence한다.
- `WorkspaceUi`
  - App이 exact pane의 one-shot terminal focus를 안전하게 arm할 수 있는 작은 API를 제공한다.

## Error handling

- Catalog에 없는 workspace/pane action은 아무 runtime command도 만들지 않는다.
- Dotenv 검증 또는 continuation admission 실패 시 restore를 성공으로 표시하지 않고 기존 sanitized warning 경로를 사용한다.
- Workspace/runtime lifetime이 달라진 pending pane focus는 즉시 폐기한다.
- `RestoreWorkspacePane`, `FocusPane`, 후속 `RestoreWorkspace` 중 delivery가 실패하면 실패를 로그로 남기고 임의 입력 버퍼링이나 empty-env 복원으로 우회하지 않는다.
- Archived agent pane은 기존 `restore_archived_pane` 경로를 그대로 사용하며 자동 재실행하지 않는다.

## Non-goals

- 모든 워크스페이스 runtime을 앱 시작 시 미리 띄우지 않는다.
- 연결 중 키 입력을 저장했다가 나중에 재생하지 않는다.
- warm runtime 수명, suspend 정책, session log 정책을 바꾸지 않는다.
- `.env` freshness 또는 keychain 검증을 생략하지 않는다.
- 런처나 sidebar의 시각 디자인을 변경하지 않는다.
- 이번 변경에서 앱을 package/rebuild/relaunch하지 않는다. 사용자가 별도로 요청할 때만 수행한다.

## Verification contract

TDD 순서는 다음 실패를 먼저 고정한다.

1. Catalog가 늦게 도착해도 저장 pane이 있는 active runtime은 restore를 정확히 한 번 예약한다.
2. 저장 세션 행 클릭은 `SwitchWorkspace`가 아니라 exact `workspace_id + pane` activation을 반환한다.
3. 동일 active workspace의 저장 pane 클릭도 no-op가 아니라 restore/focus를 만든다.
4. Cold activation은 `RestoreWorkspacePane -> FocusPane -> RestoreWorkspace` 순서를 보존한다.
5. Pending pane focus는 exact runtime에만 적용되고 첫 terminal viewport에서 한 번 소비된다.
6. Stale/unknown pane, dotenv failure, runtime replacement는 fail-closed 한다.
7. 기존 full workspace restore, cross-workspace attached restore, archived agent restore, terminal input/IME regressions은 계속 통과한다.
