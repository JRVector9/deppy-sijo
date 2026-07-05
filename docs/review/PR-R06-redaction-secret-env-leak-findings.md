# PR-R06 Findings

## Summary
- 전체 판정: Block
- Critical: 0
- High: 3
- Medium: 1
- Low: 1

확인된 안전 경로: credential 값은 keyring에 저장되고 SQLite `credentials`에는 좌표/`masked_hint`만 저장된다. `config.toml`에는 secret 필드가 없다. 세션 로그는 `redacted.ansi.log`, `redacted.plain.txt`, `events.redacted.jsonl`만 생성하며 raw 평문 로그는 기본 생성되지 않는다. 위험 경로는 plain env, agent/MCP args, encrypted raw audit blob default-on, debug logging이다. `input_encrypted_blob` default-on은 PR-R07 Finding 1과 같은 root cause라 merged summary에서는 하나의 cross-cutting High로만 집계한다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/secret/src/*`, `crates/storage/src/{db,logs,lib}.rs`, `crates/audit/src/*`, `crates/runtime/src/{in_process,command,remote}.rs`, `crates/mcp/src/transport.rs`, `crates/mcp-store/src/lib.rs`, `crates/mcp-proxy/src/hook.rs`, `crates/app/src/{config,main}.rs`, `crates/app/src/ui/{env_profiles,agents,connectors,credentials,workspace}.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, secret/token/logging `rg` searches
- 확인한 테스트: test target compile, source-level tests for DB/log secret scan, audit redaction/encryption, `SecretString`/`OAuthToken` Debug redaction

## Findings

### Finding 1
Severity: High
Area: Env persistence / env diff preview / SQLite
Files: `crates/app/src/ui/env_profiles.rs`, `crates/storage/src/db.rs`
Evidence: env UI 기본 상태는 `plain`이고 plain 선택 시 `EnvValue::Plain`이 만들어진다. storage는 key 이름과 무관하게 `env_vars.plain_value`에 저장한다. 표시 경로도 plain 값을 그대로 보여준다.
Why it matters: `API_KEY`, `DATABASE_URL`, `AUTH_TOKEN` 같은 민감 키를 plain으로 추가하면 SQLite와 env diff preview에 평문이 남는다.
Reproduction: env profile에서 key `API_KEY`, kind `plain`, value `sk-live-...`로 저장 후 SQLite `env_vars.plain_value` 조회.
Suggested fix: high-confidence secret key pattern은 secret credential만 허용하거나 repository/API 경계에서 `EnvValue::Plain` 저장을 거부한다. 예외는 unsafe allowlist와 명시 경고로 처리한다.
Suggested test: `API_KEY`/`DATABASE_URL`/`*_TOKEN` plain 저장 실패, `EnvValue::Secret { credential_id }` 성공 테스트.

### Finding 2
Severity: High
Area: Agent/MCP command args persistence / SQLite / UI display
Files: `crates/storage/src/db.rs`, `crates/mcp-store/src/lib.rs`, `crates/app/src/ui/{agents,connectors}.rs`
Evidence: agent `args_json`과 MCP server `args_json`은 입력 문자열을 그대로 JSON 직렬화해 SQLite에 저장한다. Connector UI는 "secret은 args가 아니라 자격증명/환경으로" 안내만 있고 저장/표시 경로는 args를 join해 표시한다.
Why it matters: `--api-key sk-...`, `Authorization: Bearer ...`, `--password ...`가 DB와 UI에 평문으로 남는다.
Reproduction: MCP server 또는 agent 등록 args에 `--api-key`와 실제 token을 입력한다.
Suggested fix: args 저장 전 민감 flag/value pattern을 검사해 거부하고 credential/env binding을 유도한다. 표시 경로는 redaction helper를 통과시킨다.
Suggested test: `--api-key`, `--token`, `Bearer`, `DATABASE_URL=` 포함 args 저장 거부 또는 redacted 저장/표시 테스트.

### Finding 3
Severity: High
Area: MCP audit / encrypted raw input retention
Files: `crates/audit/src/log.rs`, `crates/app/src/ui/connectors.rs`, `crates/mcp-proxy/src/hook.rs`
Evidence: `record_audit`는 `encryptor: Some(...)`이면 원본 tool input 전체를 `input_encrypted_blob`에 저장한다. GUI connector와 MCP proxy는 keyring이 있으면 기본적으로 encryptor를 전달한다.
Why it matters: 평문은 아니지만 raw input 원본이 opt-in 없이 SQLite에 보존된다. This is the same cross-cutting issue as PR-R07 Finding 1 and should be counted once in merged summaries.
Reproduction: tool input `{"token":"sk-secret"}` 호출 후 `input_redacted_json`은 redacted, `input_encrypted_blob`은 non-NULL인지 확인.
Suggested fix: user/config opt-in이 켜진 경우에만 encryptor를 전달하고 기본은 `None`으로 둔다.
Suggested test: 기본 설정에서 `input_encrypted_blob IS NULL`, opt-in에서만 non-NULL 및 decrypt roundtrip.

### Finding 4
Severity: Medium
Area: Debug logging / MCP stdout protocol violation
Files: `crates/mcp/src/transport.rs`
Evidence: MCP transport가 unsolicited server message 처리 시 `tracing::debug!(?value, ...)`로 JSON-RPC 전체 값을 로그에 남긴다. debug log가 `app.log`에 기록될 수 있다.
Why it matters: server-originated request/notification params에 token/header/tool input이 있으면 debug log에 평문으로 남을 수 있다.
Reproduction: MCP server가 outstanding request 없이 `{"params":{"Authorization":"Bearer sk-..."}}`를 stdout으로 보내고 `RUST_LOG=debug`로 실행한다.
Suggested fix: `?value`를 제거하고 method만 로깅하거나 redaction을 적용한다.
Suggested test: debug subscriber test writer에 민감 params가 포함되지 않음을 검증한다.

### Finding 5
Severity: Low
Area: Debug/Display hardening / crash-debug dump risk
Files: `crates/runtime/src/command.rs`, `crates/storage/src/db.rs`, `crates/mcp-store/src/lib.rs`, `crates/mcp/src/manager.rs`
Evidence: `RuntimeCommand`는 `Debug` derive이며 `env_plain`과 `WriteInput.bytes`를 포함한다. `EnvValue::Plain(String)`도 Debug로 평문 출력된다. `AgentConfigRow`, `McpServerRow`, `McpServerConfig` also derive `Debug` while carrying command/args that may include `--api-key`, `Bearer`, or `--password`.
Why it matters: plain env, terminal paste/input bytes, agent args, and MCP server args can include secrets and may leak through crash/debug dumps even after normal UI/log display paths are redacted.
Reproduction: `format!("{:?}", RuntimeCommand::SpawnAgent { env_plain: ... })` 또는 `format!("{:?}", EnvValue::Plain("sk-..."))`.
Suggested fix: 수동 `Debug`로 value/input bytes/args를 elide하고 key/length 같은 비민감 metadata만 표시한다, or make secret-like args impossible to persist first.
Suggested test: Debug 문자열에 `sk-`, `DATABASE_URL`, `Bearer`, `--password`, pasted password fixture가 없는지 테스트.

## Second Pass Update
- Finding 3 is raised to High and merged with PR-R07 Finding 1 for summary counting. Default app/proxy audit rows should store `input_redacted_json` only and leave `input_encrypted_blob IS NULL`; encrypted raw input must require explicit opt-in.
- Finding 4 evidence is narrowed to `crates/mcp/src/transport.rs`; `crates/app/src/main.rs` is not treated as causal evidence.
- Finding 5 debug/crash dump hardening now covers `RuntimeCommand`, `EnvValue`, `AgentConfigRow`, `McpServerRow`, and `McpServerConfig`.

## Regression Risks
- secret-like key 차단은 false positive가 가능하므로 high-confidence pattern과 explicit unsafe override가 필요하다.
- args 차단은 기존 MCP/agent 등록을 막을 수 있으므로 표시 redaction부터 단계 적용 가능.
- encrypted audit blob default-off는 조사 능력을 줄이므로 opt-in/retention/export 경고 필요.

## Recommended Build PRs
- PR-R06-FIX-1: env plain 저장 guard 및 env diff preview redaction
- PR-R06-FIX-2: agent/MCP args secret scanner와 UI 저장 차단
- PR-R06-FIX-3: MCP/audit/debug logging redaction hardening
- PR-R06-FIX-4: `input_encrypted_blob` opt-in 설정 및 default-off 테스트
- PR-R06-FIX-5: Debug redaction tests for RuntimeCommand/EnvValue

## Open Questions
- secret-like env key를 hard-block할지, explicit unsafe plain 저장을 허용할지 결정 필요.
- `input_encrypted_blob`은 기본 off인가, 관리자 opt-in인가?
- 명시적 user clipboard copy는 secret redaction/warning 대상인가?
