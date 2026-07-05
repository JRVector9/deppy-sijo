# PR-R02 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 0
- Medium: 0
- Low: 2

현재 Cargo package cycle은 재현되지 않았다. `storage -> mcp`, `mcp -> storage`, `audit -> storage`, `persist -> storage`, `mcp-store -> mcp/runtime`, `storage-core -> domain crate` edge는 확인되지 않았다. Store split은 v2.8 §17.3 기준 "부분 분리 + storage facade 유지"가 허용되는 DAG 상태이며, v3.1 PR-B01의 full store/model split 목표와 정책 정합화가 필요하다.

## Scope Reviewed
- 검토한 파일/모듈: workspace `Cargo.toml`, `crates/storage/{Cargo.toml,src/db.rs}`, `crates/storage-core/src/lib.rs`, `crates/mcp/{Cargo.toml,src/lib.rs}`, `crates/mcp-store/*`, `crates/audit/Cargo.toml`, `crates/persist/*`, `xtask/src/main.rs`, `docs/dependency-graph.md`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, `cargo tree --workspace --edges normal,build` pass, `cargo tree --workspace --edges normal,build,dev` pass, `cargo metadata --format-version 1 > target/cargo-metadata.json`, `cargo run -p xtask -- check-deps` pass
- 확인한 테스트: `cargo test -p xtask 현재_그래프는_금지edge와_순환이_없다` pass, `cargo test -p storage 전_버전_prefix_마이그레이션_스모크` pass

## Findings

### Finding 1
Severity: Low
Area: Storage facade / partial store split
Files: `crates/storage/Cargo.toml`, `crates/storage/src/db.rs`, `docs/dependency-graph.md`
Evidence: 현재 edge는 `storage -> storage-core`, `storage -> mcp-store`, `storage -> audit`, `storage -> persist`이다. `storage/src/db.rs`의 `MIGRATIONS`는 `persist::MIGRATION_SQL`, `mcp_store::MIGRATION_SQL`, `audit::MIGRATION_SQL` 및 storage 자체 migration을 한 배열에서 조립한다. `Db` facade도 mcp-store, audit, persist repo 함수를 직접 위임한다.
Why it matters: v2.8 §17.3이 storage facade를 유지하는 최종 정책이라면 현재 graph는 legal DAG이다. 남은 위험은 새 outbound edge/migration owner가 정책 없이 늘어나는 documentation/allowlist drift이다.
Reproduction: `cargo metadata --format-version 1 > target/cargo-metadata.json`, `cargo run -p xtask -- check-deps`
Suggested fix: v2.8 §17.3의 storage facade 유지가 최종 정책인지, v3.1 PR-B01 full split이 목표인지 결정한다. facade 유지가 최종이면 PR-B01 acceptance criteria를 갱신하고 allowlist를 고정한다.
Suggested test: cargo metadata 기반 graph snapshot 테스트로 `storage` 신규 outbound edge와 migration owner 변경을 리뷰하게 한다.

### Finding 2
Severity: Low
Area: MCP runtime dependency hygiene
Files: `crates/mcp/Cargo.toml`, `crates/mcp/src/lib.rs`, `crates/mcp-store/src/lib.rs`
Evidence: `crates/mcp/Cargo.toml`은 `rusqlite`를 직접 dependency로 선언하지만 `crates/mcp/src/lib.rs`는 SQL/Row/DDL이 `mcp-store` 소유라고 명시하고 실제 SQL 사용은 보이지 않는다.
Why it matters: cycle은 만들지 않지만 runtime crate가 DB crate를 이미 의존하면 SQL 저장 로직이 `mcp`에 재유입되어도 manifest diff가 발생하지 않는다.
Reproduction: `rg "rusqlite|CREATE TABLE|INSERT INTO|SELECT|UPDATE|DELETE FROM" crates/mcp -g '*.rs'`
Suggested fix: 사용하지 않는 `rusqlite` dependency를 `crates/mcp/Cargo.toml`에서 제거한다.
Suggested test: metadata 기반으로 `mcp`가 `rusqlite`, `storage`, `storage-core`, `mcp-store`를 직접 의존하지 않는지 smoke test.

## Second Pass Update
- `cargo run -p xtask -- check-deps` confirms no forbidden edge/cycle.
- Finding 1 is downgraded to Low policy/documentation alignment because v2.8 §17.3 explicitly allows a maintained storage facade as a composition/app-level store.
- Keep Finding 2 as Low dependency hygiene: remove unused `mcp -> rusqlite`.

## Regression Risks
- storage facade 축소는 `app`, `runtime`, `mcp-proxy` DB 접근 경로에 영향.
- migration owner 이동 시 기존 `user_version` 순서를 재배열하면 기존 DB가 깨질 수 있음.

## Recommended Build PRs
- PR-B01a: 현재 허용 graph를 cargo metadata 기반 xtask로 고정하고 CI에 추가
- PR-B01b: `mcp` unused `rusqlite` dependency 제거
- PR-B01c: storage facade 유지 vs full store split 정책 정리
- PR-B01d: full split 선택 시 DB schema 변경 없이 audit/env/session/mux store 순차 분리

## Open Questions
- v2.8 §17.3 storage facade 유지가 v3.1 PR-B01 full split 목표를 대체한 최종 정책인가?
- `mcp-store`가 `storage-core`에 의존하지 않고 `rusqlite::Connection`을 직접 받는 현재 방식이 최종 정책인가?
