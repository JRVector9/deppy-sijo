# PR-R00 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 1
- Medium: 2
- Low: 1

현재 구현 inventory 기준으로 pane/mux/session, workspace warm 상태, folder tree, folder tree `PathBuf` drag/drop to terminal, terminal selection/copy/paste, bracketed paste, IME commit, redacted session log 경로는 구현되어 있다. 다만 terminal-selected-text internal drag/drop paste 구현은 확인되지 않았다. RuntimeClient boundary는 workspace/terminal 흐름에는 적용되어 있지만 credential/connector/storage UI 경로에서는 부분적으로 우회한다. 또한 "active pane only"와 "visible panes in active tab"의 정책 표현이 코드와 문서 사이에서 불명확하다.

## Scope Reviewed
- 검토한 문서: v2.6 final architecture, v2.8 persistence/store improvement, v3.1 review/build split plan, review/build prompt pack
- 검토한 파일/모듈: `crates/app/src/app.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/{agents,credentials,connectors}.rs`, `crates/runtime/src/{client,command,event,in_process}.rs`, `crates/mux/src/*`, `crates/session/src/session.rs`, `crates/terminal/src/*`, `crates/pty/src/lib.rs`, `crates/storage/src/{db,logs}.rs`, `crates/secret/src/*`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, dependency spot checks and `rg` searches
- 확인한 테스트: hidden viewport 금지, Warm 상태 viewport 중단/복귀, shell quote, terminal selection text, bracketed paste, redacted log writer, redaction corpus, env secret DB 제약은 소스상 확인. 실제 전체 테스트 실행은 하지 않음.

## Findings

### Finding 1
Severity: High
Area: RuntimeClient boundary / UI boundary
Files: `crates/app/Cargo.toml`, `crates/app/src/app.rs`, `crates/app/src/ui/credentials.rs`, `crates/app/src/ui/connectors.rs`, `crates/runtime/src/client.rs`
Evidence: workspace terminal UI는 `&dyn RuntimeClient`로 명령을 보내지만 app crate는 `storage`, `secret`, `mcp`, `mcp-store`, `audit`, `auth`, `terminal`에 직접 의존한다. `App`은 `Db`, `KeyringSecretStore`, concrete `InProcessRuntimeClient`를 직접 보유한다. credential UI는 `&Db`, `&dyn SecretStore`를 직접 받아 keyring 저장/삭제를 수행하고 OAuth connector UI도 `KeyringSecretStore`를 직접 생성한다.
Why it matters: 공통 불변 원칙 "UI는 RuntimeClient만 본다"를 엄격히 적용하면 settings/credential/connector 경로가 boundary를 우회한다. 후속 remote runtime 또는 headless runtime 전환 시 UI-local side effect가 남는다.
Reproduction: `rg -n "SecretStore|KeyringSecretStore|Db|mcp-store|storage" crates/app`
Suggested fix: UI-local service 허용 범위와 RuntimeClient-only side effect 범위를 먼저 명확히 나눈다. 엄격한 원칙을 유지한다면 credential/OAuth/MCP side effect를 RuntimeCommand 또는 별도 app service boundary 뒤로 이동한다.
Suggested test: `cargo xtask check-boundary`로 `crates/app/src/ui/**`에서 `SecretStore`, `KeyringSecretStore`, `storage::Db`, `mcp_store` 직접 참조를 금지한다.

### Finding 2
Severity: Medium
Area: Pane visibility / snapshot policy
Files: `crates/runtime/src/in_process.rs`, `crates/app/src/ui/workspace.rs`
Evidence: runtime은 active tab의 모든 pane session을 watched 대상으로 삼고, UI도 active tab의 visible pane 전체를 resize/render한다. hidden tab/session viewport 금지 테스트는 존재한다.
Why it matters: 구현은 focused active pane 하나가 아니라 active tab 안의 visible panes 전체를 render/snapshot 대상으로 삼는다. split pane UX에는 맞지만 공통 문구의 "Active pane만 render"와 충돌할 수 있다.
Reproduction: split pane 2개를 같은 tab에 만들고 두 session에 출력 발생 시 두 pane 모두 viewport 대상이 된다.
Suggested fix: baseline 정책을 "active workspace/window의 active tab에 있는 visible panes만 render/snapshot한다. focused pane은 input/IME/scroll 대상이다. hidden tab/workspace는 금지"로 정정한다.
Suggested test: split pane 2개가 같은 active tab에 있을 때 두 session viewport 갱신 여부를 확정 정책에 맞게 고정한다.

### Finding 3
Severity: Medium
Area: Folder tree -> terminal path insertion quoting
Files: `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/workspace.rs`, `crates/pty/src/lib.rs`
Evidence: file tree row는 `PathBuf` drag source이고 terminal pane은 drop payload를 `RuntimeCommand::WriteInput`으로 삽입한다. quote 함수는 POSIX single-quote 방식 하나만 사용한다. Windows 기본 shell은 PowerShell이다.
Why it matters: bash/zsh 계열에서는 대체로 맞지만 PowerShell/cmd/fish까지 제품 범위에 포함하면 shell별 escape가 다르다.
Reproduction: 공백, CJK, 작은따옴표가 포함된 path를 Windows PowerShell 또는 custom shell terminal로 drag/drop한다.
Suggested fix: `shell_quote(path, shell_kind)`를 POSIX, fish, PowerShell, cmd로 분리하고 shell kind를 session metadata로 노출한다.
Suggested test: shell별 quoting table test와 no-auto-Enter test를 추가한다.

### Finding 4
Severity: Low
Area: PR-R00 smoke/automation coverage
Files: `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/workspace.rs`, `crates/terminal/src/{renderer_egui,input_mapper}.rs`
Evidence: DnD path와 UI Copy event end-to-end 자동 테스트는 확인되지 않았다. terminal selection text와 bracketed paste 단위 테스트는 있다.
Why it matters: PR-R00의 핵심은 회귀 금지 baseline 고정이므로 사용자 조작 기반 UX는 smoke harness가 필요하다.
Reproduction: `dnd_release_payload::<PathBuf>()` 처리나 `ui.ctx().copy_text(text)` 호출을 제거해도 현재 no-run 검증만으로는 잡히지 않는다.
Suggested fix: file tree `PathBuf` payload -> terminal `RuntimeCommand::WriteInput` 생성까지 검증하는 작은 smoke harness를 추가한다.
Suggested test: DnD path insert, context menu insert, selection copy, bracketed paste UI path smoke.

## Second Pass Update
- Baseline correction: implemented DnD insertion is folder-tree/sidebar `PathBuf` drop into terminal only. `crates/app/src/ui/workspace.rs` handles `dnd_release_payload::<PathBuf>()`, and `crates/app/src/ui/file_tree.rs` creates `PathBuf` drag payloads.
- No terminal-selected-text drag source/drop paste path was found. Keep "terminal internal drag/drop paste" as an open requirement until the product contract defines whether it means selected terminal text DnD or file-tree path DnD.
- Second pass confirms no app direct `alacritty_terminal`, `portable-pty`, or `SessionManager` references.

## Regression Risks
- pane/mux/session: `MuxPane.session_id: Option<SessionId>`, `LayoutNode` source of truth, pane detach/close 정책 회귀 금지.
- render/snapshot: hidden tab/workspace session은 `TerminalViewportSnapshot` 생성 금지. baseline을 유지한다면 active tab의 visible split panes는 모두 viewport/render 대상.
- folder tree UX: lazy read_dir, expanded cache, row virtualization, `PathBuf` drag source, terminal drop target, no auto Enter 유지.
- terminal copy/paste UX: selection copy consumes Copy event, selection 없는 Ctrl+C는 ETX 전송, bracketed paste, IME preedit/commit 유지.
- redacted log/secret: 기본 session log는 `redacted.ansi.log`, `redacted.plain.txt`, `events.redacted.jsonl`; raw plaintext log 금지.

## Recommended Build PRs
- PR-B00: Runtime/UI boundary clarification and enforcement
- PR-B02/PR-R03 follow-up: pane visibility policy hardening
- PR-B03: folder tree DnD shell quoting hardening
- PR-B04: terminal clipboard/IME/CJK hardening
- PR-B05/PR-R06 follow-up: redaction pipeline scan

## Open Questions
- "Active pane만 render한다" 문구를 "active workspace/window active tab visible panes만 render/snapshot"으로 문서 전반에 정규화할 것인가?
- credential/OAuth/MCP connector UI가 `SecretStore`/`Db`를 직접 쓰는 것이 허용된 app-local boundary인가?
- terminal path insert v0 지원 shell 범위는 zsh/bash만인가, PowerShell/cmd까지 즉시 포함인가?
- "terminal internal drag/drop paste" 기준 동작은 file tree drop인가, terminal selected text drag/drop인가?
