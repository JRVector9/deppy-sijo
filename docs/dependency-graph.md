# Crate 의존 그래프 (v2.8 V0~V2 적용 완료)

작성: 2026-07-04. `cargo run -p xtask -- check-deps`가 이 문서의 금지 edge를 자동 검사한다.

## 현재 로컬 의존 그래프 (V0 시점, 디렉터리명 기준)

```text
storage ──▶ core, persist, mcp, audit, secret        ← 문제의 근원(도메인 SQL을 storage가 조립)
persist ──▶ core, mux
mcp     ──▶ core, secret
audit   ──▶ core, secret
runtime ──▶ core, mux, pty, secret, session, storage, terminal, persist
app     ──▶ audit, auth, mcp, persist, platform, runtime, secret, storage, terminal
mcp-proxy ▶ storage, mcp, audit, secret, auth, core
mux/session/terminal/pty/platform/auth/secret/core: 하위 계층
```

## 순환 해소 실증 (2026-07-04, V2 후 smoke-test)

전환 전 `mcp → storage` 추가 시:

```text
error: cyclic package dependency: package `mcp` depends on itself.
```

**V2(mcp-store 분리) 후 같은 실험 → 컴파일 성공(Finished).** `mcp → mcp-store`도 성공.
순환은 구조적으로 소멸했고, runtime→store 직접 의존은 정책상 금지(xtask 가드)로만 남는다.

## v2.8 전환 후 그래프 (V2에서 적용 완료 — 목표=현실)

```text
storage-core           DB infra만 (conn/open/migration runner/backup) — 아무 도메인도 모름
mcp-store   ──▶ storage-core            mcp SQL/Row (+ tool_permission_rules, pending_approvals)
mcp(runtime)──▶ core, secret            manager/transport/proxy + PermissionPolicy/Rule — store를 모름
audit       ──▶ core, secret            감사 기록 서비스(record+AEAD, conn 주입)
persist     ──▶ core, mux               mux+session SQL + delete/reconcile (conn/tx 주입)
storage     ──▶ storage-core, mcp-store, audit, persist, core, secret
              (조립자/facade + 앱 수준 store: credentials·workspaces·env·agent_configs)
```

핵심: **runtime 성격 crate(mcp/audit-정책 등)는 store/facade를 모른다.** 저장이 필요한 주체는
최상층(app, mcp-proxy bin)이며 그들이 storage(facade) 또는 개별 store를 직접 쓴다. 따라서
`mcp → storage`류 edge는 영원히 불필요하고, xtask 금지 edge가 재발을 차단한다.

## 금지 edge (xtask FORBIDDEN_EDGES와 동기)

- `storage-core → {storage, mcp, mcp-store, audit, persist, mux, session, app, runtime, secret}`
- `mcp → {storage, storage-core, mcp-store, audit}` — 원 순환의 재발 방지 지점
- `audit → {storage, mcp, mcp-store}` / `persist → {storage, mcp, audit, runtime, app}`
- `mux → {storage, storage-core, persist}` / `session → {storage, persist, secret}`
- `storage/mcp-store → {runtime, app}` / `mcp-store → mcp` / `env-store → secret`(생기면)

## 마이그레이션 원장 (재배열 금지)

user_version 기반 전역 순서. **v1..v10의 번호·내용은 불변** — store별 concat 재배열 금지
(기존 사용자 DB 파손). 신규 마이그레이션은 항상 끝에 append.

```text
v1 credentials            v6 audit(tool_audit_logs)
v2 workspaces/env         v7 agent soft-delete
v3 agent_configs          v8 tool_permission_rules
v4 persist(mux/sessions)  v9 pending_approvals
v5 mcp(servers/tools)     v10 agent_configs proxy 컬럼
```

가드: `storage` 테스트 `전_버전_prefix_마이그레이션_스모크` (모든 k→최신 + foreign_key_check).
