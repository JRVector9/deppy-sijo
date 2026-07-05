# PR-R09 Findings

## Summary
- 전체 판정: Block
- Critical: 0
- High: 3
- Medium: 4
- Low: 0

현재 구현은 i18n readiness 기준을 충족하지 못한다. `en-US`, `ja-JP`, `zh-Hans`, `zh-Hant`용 locale infrastructure, catalog, fallback, pseudo-locale 검사, `message_id + args` 메시지 경계가 없다. 터미널 출력, path, command/args, env key, MCP server/tool name은 raw domain value로 보존되고 있으며, 문제는 hardcoded label/wrapping prose와 runtime/persistence boundary를 넘는 generated default title이다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/app/src/app.rs`, `crates/app/src/ui/*.rs`, `crates/runtime/src/{event,in_process,protocol,remote}.rs`, `crates/mcp/src/*`, `crates/storage/src/*`, `crates/audit/src/*`, `crates/session/src/*`, `crates/terminal/src/*`, `crates/platform/src/lib.rs`, `xtask/src/main.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, hardcoded string and message `rg` searches
- 확인한 테스트: i18n crate/catalog/message_id/pseudo-locale/`xtask i18n-check` 없음

## Findings

### Finding 1
Severity: High
Area: I18n infrastructure / UI strings
Files: `crates/app/src/app.rs`, `crates/app/src/ui/{settings,connectors,approvals,env_profiles,agents}.rs`, `xtask/src/main.rs`
Evidence: `crates/i18n`, locale catalog, `message_id`, `Locale`, `Fluent`, `pseudo` 검색 결과가 없다. UI는 `ui.button("설정")`, `egui::Window::new("설정")`, `ui.heading("Local MCP")`, `egui::Window::new("도구 실행 승인")`처럼 문자열을 직접 렌더링한다. `xtask`는 `check-deps`만 지원한다.
Why it matters: required locales로 UI를 표시할 수 없고 missing-key/pseudo-locale overflow 검사가 불가능하다.
Reproduction: `rg -n "message_id|Locale|Fluent|pseudo|i18n" crates xtask Cargo.toml`
Suggested fix: PR-B11에서 `crates/i18n`, Fluent catalogs, fallback, missing key check, pseudo-locale generation을 먼저 만든다. PR-B12에서 UI 문자열을 key 기반 호출로 옮긴다.
Suggested test: `cargo xtask i18n-check`로 required locale completeness, missing/unused key, pseudo-locale generation 검사.

### Finding 2
Severity: High
Area: RuntimeEvent message boundary
Files: `crates/runtime/src/event.rs`, `crates/runtime/src/in_process.rs`, `crates/app/src/ui/{workspace,agents}.rs`
Evidence: `RuntimeEvent::SpawnFailed { kind, message: String }`가 raw string을 runtime boundary로 전달하고 runtime worker가 `format!("{e:#}")`, `secret resolve 실패 ...` 같은 문자열을 만든다. UI는 이를 다시 조립해 표시한다.
Why it matters: RuntimeEvent는 문서 기준상 `message_id + args` 구조여야 한다. remote client가 자기 locale로 표시할 수 없고 이벤트 저장 시 번역 문자열이 저장될 위험이 있다.
Reproduction: 존재하지 않는 shell command 또는 누락 credential로 spawn 실패를 발생시킨다.
Suggested fix: `RuntimeEvent::SpawnFailed`를 `message_id + args + optional diagnostic` 형태로 분리한다. command/path/env key는 args로 전달하되 번역하지 않는다.
Suggested test: spawn failure 직렬화에서 `message_id`와 args를 검증하고 UI 문장이 event payload에 직접 포함되지 않음을 assert.

### Finding 3
Severity: High
Area: NotificationEvent / OS notification localization
Files: `crates/app/src/ui/notifications.rs`, `crates/app/src/app.rs`, `crates/platform/src/lib.rs`
Evidence: `NotificationEvent` 타입이 없고 `NotificationsUi`가 `NotificationItem { title: String, status }`를 저장한다. status label을 직접 한국어로 매핑하고 OS notification도 `format!("{label}: {title}")`로 보낸다.
Why it matters: background workspace/session 알림과 OS 알림이 source locale 문자열로 고정된다. 현 코드에는 notification persistence table/repo가 확인되지 않았으므로 persistence 고착은 future risk로만 취급한다.
Reproduction: session status detector가 Waiting/NeedsApproval/Error/Done을 발생시키는 session 실행.
Suggested fix: `NotificationEvent { message_id, args, workspace_id, session_id, severity/status }`를 도입하고 UI/OS notification은 client locale에서 렌더링한다.
Suggested test: status event -> notification message id 변환과 pseudo-locale OS notification summary/body 생성 검증.

### Finding 4
Severity: Medium
Area: Error string direct display
Files: `crates/app/src/ui/{settings,workspace,connectors,agents}.rs`, `crates/mcp/src/proxy.rs`
Evidence: UI가 `format!("{e:#}")`, `시작 실패: {err}`, `인자 JSON 파싱 실패: {e}`를 직접 표시한다. MCP proxy JSON-RPC error message에도 한국어 wrapper가 들어간다.
Why it matters: user-facing context와 diagnostic detail이 분리되지 않아 localization과 원문 보존 정책을 함께 만족하기 어렵다.
Reproduction: Remote TLS 시작 실패, MCP tool JSON parse 실패, backend forward 실패 발생.
Suggested fix: `UiError { message_id, args, diagnostic }` 구조를 도입하고 JSON-RPC `error.message` 정책을 stable protocol text와 GUI localization으로 분리한다.
Suggested test: user-facing string은 message id에서 렌더링되고 diagnostic은 별도 필드에 남는지 검증.

### Finding 5
Severity: Medium
Area: MCP approval dialog / Project Environment warnings
Files: `crates/app/src/ui/{approvals,connectors,env_profiles,agents}.rs`
Evidence: MCP approval popup, connector approval flow, production profile warnings가 hardcoded Korean/English mix이다.
Why it matters: approval/production warnings는 security-critical prompts라 required locales에서 이해 가능해야 한다.
Reproduction: pending MCP approval 또는 production env profile 선택.
Suggested fix: approval/environment guard copy를 high-priority i18n keys로 처리하고 server/tool/hash/env/profile/command/path 값은 unlocalized args로 둔다.
Suggested test: approval dialog and production warning locale/pseudo-locale render test.

### Finding 6
Severity: Medium
Area: CJK UI overflow / pseudo-locale coverage
Files: `crates/app/src/ui/{settings,connectors,env_profiles,approvals,file_tree}.rs`, `crates/terminal/src/*`
Evidence: 많은 window가 `.resizable(false)`이고 horizontal label/button row에 direct strings가 있다. pseudo-locale 또는 CJK screenshot tests가 없다. File tree truncation과 terminal wide char rendering은 일부 방어가 있다.
Why it matters: Japanese/Chinese translations는 길이와 glyph metrics가 달라 fixed/non-resizable window에서 overflow될 수 있다.
Reproduction: locale switch가 없어 정상 UI로는 exercise 불가. static review상 fixed windows와 missing tests 확인.
Suggested fix: pseudo-locale와 required locale screenshot/layout smoke를 추가하고 dense dialog에는 wrapping/scroll area/stable control sizing을 적용한다.
Suggested test: Settings, Agent, Env, MCP approval, Notification center, Workspace toolbar pseudo-locale visual smoke.

### Finding 7
Severity: Medium
Area: Runtime-generated mux/session titles cross i18n boundary
Files: `crates/runtime/src/in_process.rs`, `crates/runtime/src/persistence.rs`, `crates/persist/src/repo.rs`, `crates/app/src/app.rs`
Evidence: runtime creates default titles like `"셸"` and `"에이전트"`, persists them through session/mux persistence, and app notifications later reuse `pane.title`.
Why it matters: remote clients and future locales inherit Korean default titles from runtime/storage instead of rendering default session/pane titles from stable kind + ordinal. User-renamed titles should remain literal, but generated titles should be localizable.
Reproduction: spawn a default shell/agent session and inspect persisted session/pane titles before any user rename.
Suggested fix: represent generated titles as `{kind, ordinal, optional user_title}` or `message_id + args`; persist literal text only for user-supplied titles.
Suggested test: default generated title persistence stores stable kind/ordinal or message id, while renamed titles remain literal and are not translated.

## Second Pass Update
- Notification persistence concern is narrowed to future risk; current code stores/render notification strings in app memory and passes rendered OS notification text to platform APIs.
- Dynamic domain values can be stated as preserved raw: terminal output, paths, command/args, env keys, MCP server names, and MCP tool names should not be localized.
- Added Finding 7 for runtime-generated default mux/session titles crossing RuntimeEvent/MuxUpdated and persistence as Korean rendered text.

## Regression Risks
- Terminal output, shell prompts, paths, command names, env keys, MCP tool names, JSON payload values는 번역하지 않는다.
- RuntimeEvent/NotificationEvent key화가 raw log persistence나 secret leakage를 재도입하면 안 된다.
- DB/Audit에는 rendered localized string이 아니라 stable ids/enums/args를 저장한다.

## Recommended Build PRs
- PR-B11 I18n Infrastructure
- PR-B12 UI String Migration
- PR-B13 Runtime Message Localization
- PR-G02 Final I18n / CJK Gate

## Open Questions
- 현재 UI copy가 Korean이므로 `ko-KR`을 source/default locale로 둘 것인가?
- notification persistence를 추가할 경우 `message_id + args_json`을 저장할 것인가?
- raw diagnostic causes from `anyhow`는 details/log only인가?
