# Build PR Summary

## Input Findings
- PR-R06 Finding 1: secret-like env key/value가 `EnvValue::Plain`으로 SQLite `env_vars.plain_value`에 저장될 수 있음.
- PR-R06 Finding 2: agent args와 MCP server args가 `args_json`에 그대로 저장되고 UI에 그대로 표시될 수 있음.

## Scope
- DB schema 변경 없이 저장 직전 validation으로 hard-block.
- `EnvValue::Secret { credential_id }` 저장 경로는 유지.
- Agent/MCP args의 high-confidence secret-like payload 저장 차단.
- 기존 pane/folder tree/DnD/copy-paste/runtime/session 경계는 변경하지 않음.

## Changes
- `storage::Db::upsert_env_var`가 `API_KEY`, `*_TOKEN`, `DATABASE_URL`, `AUTHORIZATION`, password/secret/private-key 계열 key 또는 bearer/database/token-like plain value를 `EnvValue::Plain`으로 저장하지 않도록 거부.
- `storage::Db::insert_agent_config`가 `--api-key`, `--token`, `--password`, `Bearer`, `DATABASE_URL=` 및 high-confidence token literal이 포함된 args 저장을 거부.
- `mcp_store::insert_server`가 같은 기준으로 MCP server args 저장을 거부.
- Env/Agent/Connector UI save handler가 저장 전 validation error를 surfaced.
- 기존 DB 행에 secret-like args가 남아 있는 경우 Agent/Connector 목록 표시에서 `[REDACTED_ARGS]`로 숨김.

## Tests
- `cargo fmt --check` - pass
- `cargo test -p storage -p mcp-store` - pass
- `cargo test -p deppy-sijo env` - pass
- `cargo test -p deppy-sijo agents` - pass
- `cargo test -p deppy-sijo connectors` - pass
- `cargo test -p deppy-sijo` - pass

## Risk Notes
- Scanner는 high-confidence patterns 중심이라 모든 가능한 secret 형태를 완전 탐지하지는 않음.
- `storage`와 `mcp-store`에 유사 scanner가 중복되어 있음. 새 crate 또는 새 dependency를 만들지 않고 store crate cycle을 피하기 위한 선택.
- 이전 버전에서 이미 저장된 unsafe args는 migration으로 삭제하지 않고 UI 표시만 숨김.

## Rollback Plan
- `Db::validate_env_var_for_persistence`, `Db::validate_agent_args_for_persistence`, `mcp_store::validate_server_args_for_persistence` 호출과 helper/test additions를 되돌리면 기존 저장 동작으로 복귀.
- DB schema 변경이 없으므로 rollback migration은 필요 없음.

## Follow-up Review Requests
- PR-R06 security review: env plain guard와 args guard가 expected false-positive/false-negative 범위인지 검토.
- PR-R06 Finding 5 follow-up: `EnvValue`, `AgentConfigRow`, `McpServerRow`, `McpServerConfig`, `RuntimeCommand` Debug redaction은 별도 PR에서 처리.
