# PR-R01 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 1
- Medium: 0
- Low: 1

`crates/app`에서 `alacritty_terminal`, `portable-pty`, `SessionManager`, `TerminalBackend` 직접 참조는 발견되지 않았다. `session` crate도 secret store를 직접 참조하지 않는다. 주요 위반은 app/UI leaf module이 `SecretStore`/`KeyringSecretStore`를 직접 알고 있고 Connector UI가 MCP manager 실행, permission policy, audit persistence, secret-backed encryption, storage side effect까지 직접 수행한다는 점이다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/app/Cargo.toml`, `crates/app/src/app.rs`, `crates/app/src/ui/{credentials,connectors,agents,workspace}.rs`, `crates/runtime/src/{client,command,in_process}.rs`, `crates/session/src/*`, `crates/storage/src/logs.rs`, `crates/audit/src/{log,crypto}.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, PR-R01 지정 `rg` 검색들
- 확인한 테스트: 테스트 바이너리 컴파일만 확인

## Findings

### Finding 1
Severity: High
Area: Secret/MCP/audit/storage boundary / UI boundary
Files: `crates/app/Cargo.toml`, `crates/app/src/app.rs`, `crates/app/src/ui/credentials.rs`, `crates/app/src/ui/connectors.rs`
Evidence: `crates/app`이 `secret`, `audit`, `auth`, `mcp`, `mcp-store`, `storage` crate에 직접 의존한다. `App`은 `KeyringSecretStore`를 직접 보유하고 runtime 생성에도 주입한다. `CredentialsUi`는 `&dyn SecretStore`를 받아 `set_secret`/`delete_secret`을 직접 호출한다. `ConnectorsUi`는 `LocalMcpManager`를 생성/실행하고, permission/audit flow를 처리하며, `db.record_tool_audit(..., Some(secret_store))`로 secret-backed audit encryption까지 UI leaf module에서 수행한다.
Why it matters: secret/MCP/audit/storage side effect 지점이 UI에 분산되어 runtime/actor boundary, remote runtime 교체, secret access audit 지점이 흐려진다.
Reproduction: `rg "SecretStore|get_secret|KeyringSecretStore|LocalMcpManager|record_tool_audit|McpStore|storage::Db" crates/app`
Suggested fix: `crates/app/src/ui/*`에서 `SecretStore`/`KeyringSecretStore` 타입과 MCP/audit/storage side effect를 제거하고 credential add/delete, OAuth token store, MCP tool execution, permission/audit recording, audit encrypted blob key access를 UI 밖 service 또는 runtime-owned actor로 이동한다.
Suggested test: boundary grep test로 `crates/app/src/ui/**`의 `SecretStore`, `KeyringSecretStore`, `LocalMcpManager`, `record_tool_audit`, direct storage repo usage가 allowlist 없이 나오지 않는지 확인한다.

### Finding 2
Severity: Low
Area: RuntimeClient boundary / app composition
Files: `crates/app/src/app.rs`, `crates/app/src/ui/{agents,workspace}.rs`
Evidence: leaf UI는 `&dyn RuntimeClient`를 받아 command를 보내지만 app root는 `InProcessRuntimeClient` concrete type을 직접 import/store/construct한다.
Why it matters: `crates/app/src/app.rs`가 composition root 예외라면 leaf UI boundary 위반은 아니다. 다만 app state가 concrete in-process runtime에 묶이는 정책은 명시해야 한다.
Reproduction: `rg -n "InProcessRuntimeClient|send_command" crates/app/src/app.rs crates/app/src/ui`
Suggested fix: concrete runtime 생성/구독/shutdown을 `RuntimeWorkspaceHandle` 또는 `RuntimeHost` adapter로 격리한다.
Suggested test: `rg "InProcessRuntimeClient" crates/app/src/ui`가 no matches인지 고정하고, app의 단일 host/factory 외 출현도 allowlist로 제한한다.

## Second Pass Update
- `InProcessRuntimeClient` concrete usage is downgraded to Low/Open Policy if `crates/app/src/app.rs` is the composition root. Leaf workspace/agent UI still uses `&dyn RuntimeClient`.
- The High boundary finding is broadened from secret-only to secret/MCP/audit/storage side effects in UI leaf modules, especially `crates/app/src/ui/connectors.rs`.

## Regression Risks
- Secret boundary 이동은 credential CRUD, OAuth token 저장, redaction seed, audit encrypted blob 생성에 영향을 준다.
- Runtime handle 추상화는 file tree path insert, terminal copy/paste, pane focus, notification focus, warm workspace event drain 경로에 영향을 줄 수 있다.

## Recommended Build PRs
- PR-B00a: Remove SecretStore From App/UI
- PR-B00b: RuntimeClient Host Adapter
- PR-B00c: Boundary Check Xtask

## Open Questions
- `crates/app/src/app.rs`를 composition root 예외로 허용할 것인가?
- credential CRUD/OAuth token 저장은 RuntimeClient command로 들어가야 하는가, 별도 credential actor API가 맞는가?
