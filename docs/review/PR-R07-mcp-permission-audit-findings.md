# PR-R07 Findings

## Summary
- 전체 판정: Block
- Critical: 0
- High: 2
- Medium: 2
- Low: 0

stdout protocol strictness와 stderr redaction은 현 테스트가 잘 잡고 있다. Block 요인은 encrypted audit blob default-on, MCP env scoping 부재, MCP proxy schema hash cache revalidation 부재, Connector Center audit ordering이다. Connector Center direct call path itself rediscoveres tools before policy evaluation; schema cache risk is scoped to `deppy-mcp-proxy`.

## Scope Reviewed
- 검토한 파일/모듈: `crates/mcp/src/{transport,manager,proxy}.rs`, `crates/mcp-proxy/src/{main,hook,forwarder,cli}.rs`, `crates/audit/src/*`, `crates/storage/src/{db,logs}.rs`, `crates/mcp-store/src/lib.rs`, `crates/app/src/{app.rs,ui/connectors.rs,ui/approvals.rs,ui/agents.rs}`, `crates/runtime/src/in_process.rs`, `crates/pty/src/lib.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, PR-R07 `rg` search
- 확인한 테스트: `cargo test -p mcp -p audit -p mcp-proxy -p storage` pass

## Findings

### Finding 1
Severity: High
Area: Audit log / `input_encrypted_blob` default policy
Files: `crates/audit/src/log.rs`, `crates/app/src/ui/connectors.rs`, `crates/mcp-proxy/src/{hook,main}.rs`
Evidence: `audit::record_audit`는 `encryptor: Option<&dyn SecretStore>`로 선택 구조를 제공하지만 app 직접 호출은 `Some(secret_store)`를 고정 전달하고 proxy도 keyring 초기화 성공 시 `Some(&self.store)`를 전달한다. opt-in 설정/CLI flag/UI 설정은 보이지 않는다.
Why it matters: 원본 tool input 전체가 keyring key로 복호 가능한 형태로 DB에 기본 저장된다. 문서의 기본값은 redacted audit이고 raw input 보존은 선택 기능이다.
Reproduction: keyring 가능한 환경에서 Connector Center 또는 `deppy-mcp-proxy`로 tool 호출 후 `tool_audit_logs.input_encrypted_blob IS NOT NULL` 확인.
Suggested fix: 기본 경로는 `record_tool_audit(..., None)`을 넘기고, raw input 암호화 보존은 명시 opt-in일 때만 `Some(secret_store)`를 넘긴다.
Suggested test: 기본 앱/proxy 호출 모두 blob NULL, opt-in에서만 non-NULL 및 decrypt 가능.

### Finding 2
Severity: High
Area: MCP env injection / secret scope
Files: `crates/mcp/src/{manager,transport}.rs`, `crates/mcp-store/src/lib.rs`, `crates/app/src/ui/agents.rs`, `crates/runtime/src/in_process.rs`, `crates/pty/src/lib.rs`
Evidence: `McpServerConfig`에는 env field가 없고 `StdioClient::spawn`은 `Command::new(command).args(args)`만 설정한다. `env_clear`나 explicit env injection이 없다. agent가 `deppy-mcp-proxy`를 spawn하면 proxy/backend MCP subprocess가 agent env를 상속할 수 있다. Connector Center 직접 호출은 Project Env profile 기반 MCP env 주입이 없다.
Why it matters: 직접 실행 경로는 scoped env를 주입하지 못하고, agent-proxy 경로는 agent env 전체를 backend MCP server에 노출할 수 있다.
Reproduction: agent env profile에 secret env를 설정하고 MCP proxy backend를 env 출력 mock으로 둔다.
Suggested fix: MCP server별 env binding을 Project Environment Manager에서 명시 resolve하고 backend spawn 직전에만 secret을 keyring에서 resolve한다. backend `Command`는 controlled baseline env + MCP scoped env만 받는다.
Suggested test: backend가 selected MCP scoped env만 받고 agent env secret을 상속하지 않으며 missing credential은 spawn 전 fail-closed.

### Finding 3
Severity: Medium
Area: Schema hash reapproval / MCP proxy cache
Files: `crates/mcp-proxy/src/hook.rs`
Evidence: proxy hook은 live backend schema를 최초 discover 후 `schema_cache`에 저장하고 이후 호출은 cache를 재사용한다. Allow rule 자동 통과는 cached `schema_hash_for()`와 저장 hash 비교에 의존한다.
Why it matters: 장시간 proxy session 중 backend schema가 바뀌면 저장된 Allow가 계속 통과할 수 있다.
Reproduction: 같은 proxy process에서 schema A로 AllowAlways 저장 후 backend가 schema B를 반환하도록 바꾸고 다시 호출한다.
Suggested fix: Allow 자동 통과 직전 target tool schema 재조회 또는 TTL/config version/backend mtime invalidation을 둔다. 재조회 실패는 fail-closed.
Suggested test: 같은 `DbPermissionHook` 인스턴스에서 schema A->B 변경 시 두 번째 호출이 pending approval 또는 deny가 되는지 테스트.

### Finding 4
Severity: Medium
Area: Connector Center direct call / audit ordering
Files: `crates/app/src/ui/connectors.rs`, `crates/audit/src/log.rs`
Evidence: Connector Center direct 실행은 Submit 시 raw `inv.input`으로 policy/approval flow를 시작한다. 사용자가 승인하면 `run_tool`이 먼저 audit을 기록하고 그 뒤 JSON object인지 파싱한다.
Why it matters: invalid JSON/non-object input이 승인 및 audit decision으로 남지만 실제 `tools/call`은 실행되지 않을 수 있다. encrypted blob default-on과 결합하면 실행되지 않은 raw input도 보존된다.
Reproduction: tool input에 `{bad` 또는 `[]` 입력 후 승인한다.
Suggested fix: policy/approval/audit 전에 input을 JSON object로 검증한다. invalid input은 local validation failure로 처리한다.
Suggested test: invalid JSON/non-object는 approval dialog, permission rule, tool audit row를 만들지 않는지 확인한다.

## Second Pass Update
- `input_encrypted_blob` default-on is duplicated with PR-R06 Finding 3; merged summaries count it once as a cross-cutting High.
- Connector Center direct calls run `discover_tools` during prepare before policy evaluation, so schema revalidation risk remains scoped to `deppy-mcp-proxy` session cache.
- Default app/proxy audit acceptance criteria should assert `input_redacted_json` is present and `input_encrypted_blob IS NULL` unless explicit opt-in is enabled.

## Regression Risks
- `LocalMcpManager::call_tool`은 저수준 API라 새 호출자는 permission wrapper를 거쳐야 한다.
- MCP env `env_clear`는 `PATH`, `HOME`, 인증 helper 등 baseline allowlist가 필요하다.
- schema hash 재조회는 subprocess spawn 비용과 UX 지연을 늘릴 수 있다.

## Recommended Build PRs
- PR-B06a: Audit encrypted blob default-off hardening
- PR-B06b: MCP server scoped env injection via Project Environment Manager
- PR-B06c: MCP proxy schema hash cache invalidation / per-call revalidation
- PR-B06d: Connector Center input validation before approval/audit

## Open Questions
- `input_encrypted_blob`은 기본 활성 의도인가, 문서대로 명시 opt-in인가?
- MCP backend process가 agent env를 상속해도 되는가?
- Connector Center direct tool 실행을 유지할지 proxy permission layer로 통합할지 결정 필요.
