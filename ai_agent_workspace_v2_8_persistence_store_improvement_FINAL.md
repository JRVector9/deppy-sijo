# v2.5 구현 후 발견된 Crate 순환 의존 문제 개선 최종 문서

작성일: 2026-07-04  
대상: Rust AI Agent Workspace v2.5 구현 코드베이스  
문서 성격: v2.5 전체 설계 대체 문서가 아니라, **이미 구현된 v2.5에서 발견된 persistence crate 순환 의존 문제를 해결하기 위한 별도 최종 보완 문서**  
상태: 개발 착수 가능

---

## 0. 이 문서의 목적

v2.5 설계는 이미 다음 핵심 구조를 전제로 구현되었다.

```text
App UI
 → RuntimeClient
 → Runtime Boundary
 → Mux Runtime
 → Session Runtime
 → TerminalBackend
 → PtyBackend
```

v2.5의 핵심 불변 원칙도 유지한다.

```text
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux / session / env / terminal / pty를 조율한다.
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
5. TerminalViewportSnapshot은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다.
7. Session은 secret store를 직접 모른다.
```

이번 문서는 위 구조를 바꾸지 않는다.  
이 문서는 **v2.5 구현 중 발견된 crate 순환 의존 문제**만 해결한다.

발견된 문제:

```text
error: cyclic package dependency: package `mcp` depends on itself
```

원인:

```text
현재 코드:
  storage → mcp
  storage → audit
  storage → persist

v2.5 목표 의존:
  mcp → storage

결과:
  storage → mcp → storage
```

따라서 v2.5 구현 코드에 `mcp → storage`를 그대로 추가하면 Cargo cyclic package dependency가 발생한다.

---

## 1. 최종 판단

이 문제의 장기적으로 안전한 해결책은 **store crate 분리**다.

최종 방향:

```text
storage-core
  DB infra only

*-model
  저장 가능한 순수 model

*-store
  SQL / Row / Repository

persist
  cross-store orchestration

runtime
  domain + store + mux/session/env/mcp 조립
```

최종적으로 다음 순환을 제거한다.

```text
storage → mcp → storage
storage → audit → storage
storage → persist → storage
storage → mux → storage
```

---

## 2. 현재 상태와 문제

## 2.1 현재 현실

현재 구현에서 `storage`가 아래 crate를 끌어다 쓰고 있다.

```text
storage ──▶ mcp
storage ──▶ audit
storage ──▶ persist
```

예상되는 실제 사용 예:

```text
storage/db.rs
  → mcp::MIGRATION_SQL
  → mcp::McpServerRow
  → audit::AuditRecord
  → persist::delete_workspace_data
```

즉 현재 `storage`는 DB infra만 가진 crate가 아니라, 각 domain crate의 SQL/Row/logic을 끌어다 조립하는 형태다.

## 2.2 v2.5 목표와 충돌

v2.5에서는 장기적으로 다음 방향을 의도했다.

```text
mcp ──▶ storage
```

하지만 현재 구조와 결합하면:

```text
mcp ──▶ storage ──▶ mcp
```

가 되어 cycle이 생긴다.

Cargo는 crate 순환 의존을 허용하지 않으므로 컴파일이 실패한다.

---

## 3. 개선 원칙

## 3.1 유지할 것

v2.5의 제품 구조는 유지한다.

```text
- UI / Runtime 분리 유지
- RuntimeClient 구조 유지
- Mux Runtime 유지
- Session Runtime 유지
- TerminalBackend abstraction 유지
- Redacted log 기본값 유지
- Project Environment Manager 유지
- MCP/OAuth 단계적 확장 유지
```

## 3.2 바꿀 것

persistence crate 구조만 바꾼다.

기존 목표:

```text
mcp → storage
storage → repository implementations
```

수정 목표:

```text
mcp-runtime
  MCP 실행 로직

mcp-model
  MCP 저장 가능한 순수 model

mcp-store
  MCP SQL / Row / Repository

storage-core
  DB infra only

runtime
  mcp-runtime + mcp-store 조립
```

## 3.3 금지할 것

```text
- mcp-runtime → storage-core 직접 의존 금지
- mcp-runtime → mcp-store 직접 의존 금지
- mcp-store → mcp-runtime 의존 금지
- storage-core → mcp-* 의존 금지
- audit-store → runtime/app 의존 금지
- mux-store → mux runtime 의존 금지
- env-store → secret store 직접 의존 금지
- session-store → secret store 직접 의존 금지
```

---

## 4. 최종 crate 구조

```text
crates/
 ├─ storage-core/
 │   ├─ db.rs
 │   ├─ connection.rs
 │   ├─ transaction.rs
 │   ├─ migration.rs
 │   ├─ sqlite.rs
 │   └─ error.rs
 │
 ├─ mcp-model/
 │   ├─ ids.rs
 │   ├─ server.rs
 │   ├─ tool.rs
 │   ├─ transport.rs
 │   └─ trust.rs
 │
 ├─ mcp-runtime/
 │   ├─ local_server.rs
 │   ├─ remote_client.rs
 │   ├─ stdio_transport.rs
 │   ├─ streamable_http.rs
 │   ├─ jsonrpc.rs
 │   ├─ permission_policy.rs
 │   └─ schema_hash.rs
 │
 ├─ mcp-store/
 │   ├─ rows.rs
 │   ├─ repo.rs
 │   ├─ migrations.rs
 │   └─ mapper.rs
 │
 ├─ audit-model/
 │   ├─ record.rs
 │   ├─ decision.rs
 │   ├─ target.rs
 │   └─ redacted_input.rs
 │
 ├─ audit-store/
 │   ├─ rows.rs
 │   ├─ repo.rs
 │   ├─ migrations.rs
 │   └─ mapper.rs
 │
 ├─ mux-model/
 │   ├─ ids.rs
 │   ├─ layout_node.rs
 │   ├─ pane.rs
 │   ├─ tab.rs
 │   └─ window.rs
 │
 ├─ mux/
 │   ├─ workspace.rs
 │   ├─ window.rs
 │   ├─ tab.rs
 │   ├─ pane.rs
 │   ├─ layout_tree.rs
 │   ├─ focus.rs
 │   ├─ attach.rs
 │   └─ events.rs
 │
 ├─ mux-store/
 │   ├─ rows.rs
 │   ├─ repo.rs
 │   ├─ migrations.rs
 │   └─ mapper.rs
 │
 ├─ env-store/
 │   ├─ rows.rs
 │   ├─ repo.rs
 │   ├─ migrations.rs
 │   └─ mapper.rs
 │
 ├─ session-store/
 │   ├─ rows.rs
 │   ├─ repo.rs
 │   ├─ migrations.rs
 │   └─ mapper.rs
 │
 ├─ persist/
 │   ├─ delete_workspace.rs
 │   ├─ export_workspace.rs
 │   ├─ import_workspace.rs
 │   └─ vacuum.rs
 │
 └─ runtime/
     ├─ command.rs
     ├─ event.rs
     ├─ in_process.rs
     ├─ router.rs
     └─ composition.rs
```

---

## 5. 최종 의존 방향

## 5.1 정상 의존 방향

```text
storage-core
  → core

mcp-model
  → core

mcp-runtime
  → core
  → mcp-model

mcp-store
  → core
  → storage-core
  → mcp-model

audit-model
  → core

audit-store
  → core
  → storage-core
  → audit-model

mux-model
  → core

mux
  → core
  → mux-model

mux-store
  → core
  → storage-core
  → mux-model

env-store
  → core
  → storage-core
  → env

session-store
  → core
  → storage-core
  → session

persist
  → core
  → storage-core
  → mcp-store
  → audit-store
  → mux-store
  → env-store
  → session-store

runtime
  → core
  → mux
  → session
  → env
  → mcp-runtime
  → mcp-store
  → audit-store
  → mux-store
  → env-store
  → session-store
  → persist
```

## 5.2 금지 의존 방향

```text
storage-core → mcp-model       금지
storage-core → mcp-runtime     금지
storage-core → audit-model     금지
storage-core → mux             금지
storage-core → persist         금지

mcp-runtime → mcp-store        금지
mcp-runtime → storage-core     금지
mcp-model → mcp-store          금지

audit-model → audit-store      금지
mux → mux-store                금지
env → env-store                금지
session → session-store        금지

store crate → runtime          금지
store crate → app              금지
store crate → egui             금지
```

---

## 6. crate별 책임

## 6.1 storage-core

책임:

```text
- SQLite connection
- transaction helper
- migration runner
- PRAGMA 설정
- WAL / foreign_keys 설정
- common DB error type
```

비책임:

```text
- MCP SQL
- Audit SQL
- Mux SQL
- Env SQL
- Session SQL
- Workspace delete orchestration
- domain business logic
```

## 6.2 mcp-model

책임:

```text
- McpServerId
- McpToolId
- McpServerConfig
- McpToolDefinition
- McpTransportKind
- ToolTrustLevel
```

비책임:

```text
- stdio process 실행
- Streamable HTTP client
- SQLite row/repo
- audit 저장
```

## 6.3 mcp-runtime

책임:

```text
- local stdio MCP server 실행
- streamable HTTP client
- JSON-RPC protocol
- tool call runtime
- permission request 생성
```

비책임:

```text
- SQLite 저장
- migration SQL
- row type
```

## 6.4 mcp-store

책임:

```text
- mcp_servers table
- mcp_tools table
- McpServerRow
- McpToolRow
- McpRepository
- row ↔ mcp-model mapper
```

의존:

```text
mcp-store → storage-core
mcp-store → mcp-model
```

의존 금지:

```text
mcp-store → mcp-runtime
```

## 6.5 audit-model / audit-store

```text
audit-model:
  AuditRecord
  AuditDecision
  AuditTarget
  RedactedInput

audit-store:
  tool_audit_logs SQL
  AuditRecordRow
  AuditRepository
  encrypted input blob persistence
```

## 6.6 mux-model / mux / mux-store

```text
mux-model:
  LayoutNode
  PaneId
  TabId
  WindowId

mux:
  focus
  attach/detach
  layout mutation
  pane/session mapping runtime

mux-store:
  mux_windows
  mux_tabs
  mux_layouts
  mux_panes
  layout_json persistence
```

중요:

```text
mux-store는 mux runtime을 모른다.
mux-store는 mux-model만 안다.
```

## 6.7 env-store

책임:

```text
- env_profiles
- env_vars
- env profile repository
- env var repository
```

금지:

```text
- keyring 접근 금지
- secret 값 읽기 금지
- API key 복호화 금지
```

Secret 값 resolve는 runtime orchestration에서만 한다.

## 6.8 persist

책임:

```text
- workspace delete
- workspace export
- workspace import
- cross-store transaction orchestration
```

---

## 7. migration ownership

## 7.1 원칙

```text
storage-core는 migration runner만 제공한다.
각 store crate가 자기 migration SQL을 소유한다.
runtime/bootstrap이 migration 목록을 aggregate한다.
```

## 7.2 예시

```rust
let migrations = [
    mcp_store::migrations(),
    audit_store::migrations(),
    mux_store::migrations(),
    env_store::migrations(),
    session_store::migrations(),
].concat();

storage_core::MigrationRunner::run(db, migrations)?;
```

금지:

```text
storage-core가 mcp-store/audit-store/mux-store를 의존하는 것
```

---

## 8. transaction boundary

Cross-store 작업은 반드시 바깥 orchestration 계층이 transaction을 소유한다.

## 8.1 잘못된 구조

```text
mcp-store.delete_by_workspace()
  자체 transaction

audit-store.delete_by_workspace()
  자체 transaction

mux-store.delete_by_workspace()
  자체 transaction
```

중간 실패 시 일부만 삭제될 수 있다.

## 8.2 올바른 구조

```rust
storage_core::transaction(|tx| {
    audit_store.delete_by_workspace(tx, workspace_id)?;
    mcp_store.delete_by_workspace(tx, workspace_id)?;
    mux_store.delete_by_workspace(tx, workspace_id)?;
    session_store.delete_by_workspace(tx, workspace_id)?;
    env_store.delete_by_workspace(tx, workspace_id)?;
    workspace_store.delete(tx, workspace_id)?;
    Ok(())
})?;
```

store repo API는 transaction을 인자로 받는다.

```rust
fn delete_by_workspace(
    &self,
    tx: &storage_core::Transaction,
    workspace_id: WorkspaceId,
) -> anyhow::Result<()>;
```

---

## 9. MCP server env 저장 개선

v2.5에는 `mcp_servers`에 env 관련 컬럼을 추가할지 모호한 부분이 있었다.  
v2.8에서는 MCP env도 Project Environment Manager로 통일한다.

## 9.1 새 테이블

```sql
CREATE TABLE mcp_server_env_profiles (
    server_id TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'mcp',
    created_at TEXT NOT NULL,
    PRIMARY KEY(server_id, profile_id),
    FOREIGN KEY(server_id) REFERENCES mcp_servers(id),
    FOREIGN KEY(profile_id) REFERENCES env_profiles(id)
);
```

## 9.2 정책

```text
agent env:
  agent config + env profile binding

mcp env:
  mcp_server_env_profiles

session one-shot env:
  non-persistent

manual launch override:
  non-persistent
```

`mcp_servers.env_json` / `mcp_servers.env_credentials_json`는 사용하지 않는다.

---

## 10. PR 계획

## PR-P0 — Dependency Graph Smoke Test

목표:

```text
현재 crate graph와 cycle 위험을 확인한다.
```

작업:

```bash
cargo check --workspace --all-targets
cargo test --workspace --no-run
cargo tree --workspace --edges normal,build
cargo tree --workspace --edges normal,build,dev
cargo metadata --format-version 1 > target/cargo-metadata.json
```

완료 기준:

```text
- 현재 cycle 재현 여부 기록
- storage → mcp/audit/persist 여부 기록
- mcp/audit/persist → storage 추가 금지 명시
- docs/dependency-graph.md 작성
```

## PR-P1 — Introduce storage-core

목표:

```text
DB infra를 storage-core로 분리한다.
```

완료 기준:

```text
- storage-core는 domain crate를 모름
- 기존 빌드 green
- 기존 storage crate는 storage-core를 사용
```

## PR-P2a — Extract mcp-model

목표:

```text
MCP 저장 가능한 순수 타입을 mcp-model로 분리한다.
```

완료 기준:

```text
- mcp-model은 storage/mcp-runtime을 모름
- McpServerConfig / McpToolDefinition 이동
```

## PR-P2b — Extract mcp-store

목표:

```text
MCP SQL/Row/Repo를 mcp-store로 이동한다.
```

완료 기준:

```text
- mcp-store → storage-core
- mcp-store → mcp-model
- mcp-store → mcp-runtime 금지
- storage → mcp 의존 제거
```

## PR-P2c — Move MCP migrations/rows

목표:

```text
mcp::MIGRATION_SQL, mcp::McpServerRow 등을 mcp-store로 완전 이동한다.
```

완료 기준:

```text
- mcp crate에 SQL/Row 없음
- cargo check --workspace green
```

## PR-P3a — Extract audit-model

목표:

```text
Audit domain type을 audit-model로 분리한다.
```

완료 기준:

```text
- audit-model은 store를 모름
```

## PR-P3b — Extract audit-store

목표:

```text
Audit SQL/Row/Repo를 audit-store로 이동한다.
```

완료 기준:

```text
- storage → audit 제거
- audit-store → audit-model / storage-core
```

## PR-P4 — Extract mux-model and mux-store

목표:

```text
LayoutNode 등 저장 가능한 mux model을 runtime mux에서 분리한다.
```

완료 기준:

```text
- mux → mux-model
- mux-store → mux-model
- store layer가 mux runtime을 모름
```

## PR-P5 — Extract env-store and session-store

목표:

```text
env/session persistence를 store crate로 분리한다.
```

완료 기준:

```text
- env crate는 storage 구현체를 모름
- session crate는 storage 구현체를 모름
- env-store는 secret store를 모름
```

## PR-P6 — Persist Orchestrator

목표:

```text
workspace delete/export/import를 persist crate로 정리한다.
```

완료 기준:

```text
- persist가 store repo들을 조율
- cross-store 작업은 단일 transaction
- store crate는 persist를 모름
```

## PR-P7 — Remove or Freeze storage facade

목표:

```text
기존 storage facade를 제거하거나 deprecated bridge로 고정한다.
```

완료 기준:

```text
- 새 repository 추가 금지
- 새 migration 추가 금지
- runtime이 필요한 store crate를 직접 주입
- no cycle
```

---

## 11. xtask 검증

## 11.1 cargo graph 검사

```bash
cargo xtask check-deps
```

검사:

```text
- workspace package graph 파싱
- 금지 edge 검사
- A → B → A cycle 검사
- storage-core가 domain crate를 의존하는지 검사
- dev-dependency cycle 검사
```

## 11.2 DB migration 검사

```bash
cargo xtask smoke-db-migrations
```

검사:

```text
- 빈 DB → 최신 schema
- 구버전 DB fixture → 최신 migration
- PRAGMA foreign_key_check
- WAL mode 확인
- destructive migration copy-table 확인
```

## 11.3 repository 테스트

```text
mcp-store:
  insert/list/update/delete mcp server
  tools list cache

audit-store:
  redacted audit insert
  encrypted blob optional insert

mux-store:
  layout save/load
  pane/session attachment

env-store:
  secret env는 credential_id만 저장
  plain env는 plain_value 저장

session-store:
  session create/update/status
```

---

## 12. test-support crate

dev-dependency cycle을 막기 위해 test helper는 별도 crate로 둔다.

```text
test-support
  fixtures
  fake ids
  temp db
  sample rows
```

의존:

```text
test-support → core
test-support → mcp-model
test-support → audit-model
test-support → mux-model
```

금지:

```text
domain crate들이 서로를 dev-dependency로 물고 cycle 만드는 것
```

---

## 13. 기존 v2.5와의 관계

v2.5는 폐기하지 않는다.  
다만 persistence 계층만 v2.8로 교체한다.

## 13.1 유지

```text
- UI는 RuntimeClient만 본다.
- Runtime Boundary 유지
- Mux Runtime 유지
- Session Runtime 유지
- TerminalBackend abstraction 유지
- AlacrittyBackend first / LibGhosttyBackend later 유지
- Project Environment Manager 유지
- redacted logs 기본값 유지
- resource policy 유지
- MCP/OAuth 단계적 확장 유지
```

## 13.2 변경

```text
기존:
  storage가 모든 repository implementation을 갖고,
  mcp가 storage를 의존하는 구조

수정:
  storage-core는 DB infra만 담당
  domain별 store crate가 SQL/Row/Repo 소유
  mcp-runtime은 store를 모름
  mcp-store는 mcp-model만 의존
  runtime/persist가 domain + store를 조립
```

---

## 14. 장점

```text
- Cargo cycle 제거
- SQL ownership 명확
- Row type ownership 명확
- Repository ownership 명확
- domain runtime과 persistence 구현 분리
- store crate별 테스트 쉬움
- persist orchestration 명확
- mux runtime과 mux persistence 분리
- remote/headless runtime에 유리
- v2.5 핵심 불변 원칙과 충돌하지 않음
```

---

## 15. 비용과 리스크

```text
- crate 수 증가
- 초반 리팩터링 비용 큼
- runtime composition 코드 증가
- PR을 잘게 나누지 않으면 회귀 위험
- store/model/runtime naming discipline 필요
```

대응:

```text
- PR-P0~P7로 점진 분리
- 각 PR cargo check --workspace green
- xtask check-deps로 금지 edge 자동 검사
- storage facade는 bridge로만 사용하고 새 기능 금지
```

---

## 16. 최종 판단

v2.8은 v2.5의 제품 설계를 유지하면서, smoke-test에서 확인된 crate cycle 문제를 해결하는 보완 설계다.

최종 결론:

```text
store crate 분리는 장기적으로 가장 안전한 해결이다.

단, mcp-store → mcp-runtime 같은 구조는 피하고,
mcp-store → mcp-model 구조로 둔다.

storage-core는 DB infra만 담당한다.
각 store crate가 자기 SQL/Row/Repo를 가진다.
persist가 cross-store 작업을 단일 transaction으로 조율한다.
runtime이 전체를 조립한다.
```

이 구조로 전환하면 다음 순환을 제거할 수 있다.

```text
storage → mcp → storage
storage → audit → storage
storage → persist → storage
storage → mux → storage
```

따라서 v2.5 기준 개발을 계속하되, persistence 계층은 본 문서(v2.8)의 store crate 분리 계획을 우선 반영한다.
