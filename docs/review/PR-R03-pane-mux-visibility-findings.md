# PR-R03 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 0
- Medium: 3
- Low: 1

hidden pane의 새 viewport 생성은 runtime 테스트로 방어되어 있다. 구현 정책은 focused active pane 하나가 아니라 active workspace/window의 active tab 안 visible panes 전체를 render/snapshot 대상으로 삼는다. 이 정책은 split pane UX에 필요하므로 focused-only 수정은 회귀다. UI stale viewport cache와 remote server/client delta baseline이 hidden 전환 뒤 snapshot Arc를 보존할 수 있다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/runtime/src/in_process.rs`, `crates/runtime/src/remote.rs`, `crates/app/src/ui/workspace.rs`, `crates/mux/src/*`, `crates/session/src/session.rs`, `crates/terminal/src/*`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, PR-R03 지정 `rg` 검색들
- 확인한 테스트: `cargo test -p runtime 비활성_pane은_viewport_push_안됨` pass, `cargo test -p runtime warm에서_viewport_중단_active복귀시_재개` pass, `cargo test -p runtime status_화면_패턴_hidden에서_snapshot_없이_감지` pass, `cargo test -p mux` pass

## Findings

### Finding 1
Severity: Medium
Area: Pane visibility / active render policy
Files: `crates/runtime/src/in_process.rs`, `crates/app/src/ui/workspace.rs`
Evidence: `watched_sessions()`는 active tab의 모든 pane session을 반환하고 `push_watched_viewports()`/`pump_sessions()`는 그 전체에 대해 `take_snapshot()`을 호출한다. UI도 active tab `LayoutNode` 전체를 재귀 순회해 각 pane에서 `renderer_egui::draw()`를 호출한다.
Why it matters: "Active pane만 render"를 focused `active_pane` 하나로 해석하면 현재 구현은 불일치한다. 하지만 focused-only render로 바꾸면 split pane 비포커스 pane이 얼어 보인다. 현재 구현은 "active workspace/window active tab 안의 visible panes 전체" 정책으로 정규화해야 한다.
Reproduction: active tab에서 split pane 2개를 만들고 둘 다 출력시키면 focused pane이 아닌 pane도 Viewport push/glyph draw 대상이다.
Suggested fix: 제품 정책을 "active workspace/window active tab의 visible panes만 render/snapshot; focused pane은 input/IME/scroll 대상"으로 명시한다.
Suggested test: split tab에서 pane A/B 출력이 새 `RuntimeEvent::Viewport`와 draw로 이어지는지 정책에 맞게 검증한다.

### Finding 2
Severity: Medium
Area: UI hidden snapshot cache / stale Viewport handling
Files: `crates/app/src/ui/workspace.rs`
Evidence: `MuxUpdated` 처리에서는 active tab 밖 session의 `view.snapshot = None`으로 hidden render cache를 버리지만 같은 drain batch에서 뒤따라온 `RuntimeEvent::Viewport`는 `session_alive()`만 통과하면 다시 `view.snapshot = Some(...)`으로 저장된다.
Why it matters: 새 hidden snapshot 생성은 아니지만 hidden 전환 직후 stale viewport가 hidden session cache를 되살릴 수 있다.
Reproduction: 이벤트 순서를 `[MuxUpdated(active_tab=B), Viewport(session_of_tab_A)]`로 `WorkspaceUi::handle_events()`에 전달한다.
Suggested fix: Viewport 수신 시 현재 mux의 active tab visible set에 속한 session인지 확인하고 visible이 아니면 snapshot을 저장하지 않는다.
Suggested test: `MuxUpdated` 후 hidden session `Viewport`를 주입해 `view.snapshot`이 `None`으로 유지되는지 검증한다.

### Finding 3
Severity: Medium
Area: Remote delta baseline / hidden render cache retention
Files: `crates/runtime/src/remote.rs`
Evidence: remote delta encoder는 Viewport 전송 시 server-side `last_sent`에 `Arc<TerminalViewportSnapshot>` baseline을 보관한다. remote client reconstruction cache `recon`도 snapshot baseline을 보관한다. 둘 다 `SessionExited`에서는 제거하지만 `MuxUpdated`로 hidden이 되는 경우 visible set 기준 prune이 없다.
Why it matters: remote 연결이 오래 유지되면 한 번 visible이었던 hidden pane들의 마지막 full snapshot Arc가 server/client remote baseline에 남을 수 있다.
Reproduction: remote client 연결 상태에서 여러 tab/pane을 visible 후 hidden으로 전환한다.
Suggested fix: Delta pump가 `MuxUpdated`를 볼 때 active tab visible session set을 계산해 server `last_sent`와 client `recon`에서 보이지 않는 session baseline을 제거한다.
Suggested test: remote encode/reconstruct 테스트에서 `Viewport(A)` 후 `MuxUpdated(active_tab=B)` 처리 시 server `last_sent`와 client `recon`에서 A가 제거되는지 검증한다.

### Finding 4
Severity: Low
Area: Snapshot API guard strength
Files: `crates/session/src/session.rs`, `crates/terminal/src/alacritty_backend.rs`
Evidence: `Session::take_snapshot()` 주석은 hidden pane에 대해 호출하지 않는 것이 caller 책임이라고 명시한다. `AlacrittyBackend::viewport_snapshot()`은 visibility flag 없이 항상 snapshot을 만든다.
Why it matters: 현재 runtime call site는 guard를 두지만 hidden snapshot 금지가 타입/API 수준이 아니라 관례에 의존한다.
Reproduction: `Session::set_visible(false)` 또는 backend `set_visible(false)` 후 `take_snapshot()`/`viewport_snapshot()`을 직접 호출한다.
Suggested fix: `Session`에 visible 상태를 보관하고 `take_snapshot()`에서 false면 `None`을 반환하거나 runtime 전용 wrapper로 중앙화한다.
Suggested test: hidden 전환한 `Session`에 대해 `take_snapshot()`이 `None`인지 테스트.

## Second Pass Update
- Finding 1 is policy wording, not an implementation defect. Build Track must not change to focused-pane-only rendering without an explicit product decision.
- Finding 2 remains real but is stale cache retention, not new hidden snapshot generation. `Viewport` handling should filter by current visible set, not only `session_alive()`.
- Finding 3 now covers both remote server `last_sent` and remote client `recon` snapshot baselines.

## Regression Risks
- focused pane만 render하면 split pane 비포커스 출력이 얼어 보일 수 있다.
- UI Viewport visible filter는 tab 복귀 직후 공백을 만들 수 있다.
- remote `last_sent` prune은 재가시화 시 keyframe 빈도를 늘린다.

## Recommended Build PRs
- PR-R03-FIX-1: "active pane" 용어 확정
- PR-R03-FIX-2: hidden session Viewport cache 재삽입 방지
- PR-R03-FIX-3: remote delta `last_sent` baseline prune
- PR-R03-FIX-4: `Session::take_snapshot()` visibility guard 중앙화

## Open Questions
- "Active pane" 문구를 "active workspace/window active tab visible panes"로 설계 문서와 prompt pack 전반에 정규화할 것인가?
- hidden session의 마지막 `TerminalViewportSnapshot`을 summary 용도로 보관해도 되는가?
