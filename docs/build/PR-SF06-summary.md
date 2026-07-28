# PR-SF06 Integration And Release Evidence

작성일: 2026-07-28
통합 브랜치: `codex/sf06-integration-evidence`
검증 커밋: `3cb6f38187465123f82ad648395e1b3e25aae9d1`

## 1. Input Findings

- SF01: warm/hidden replay와 workspace Git-label 캐시의 무제한 보존.
- SF02: web-push fresh/retry/in-flight session job의 무제한 admission 및 중복 경합.
- SF03: archived restore 검증/rebind 실패 전 persistent row와 runtime marker를 소비할 수 있는 순서.
- SF04: exported remote client의 TCP connect deadline과 silent-peer liveness deadline 부재.
- SF05: session log와 scrollback archive 탐색의 aggregate entry-count 상한 부재.

## 2. Scope

- 동일 base `c99c2cc0e4b1705b23e1ed71f36e0ac983d322aa`에서 시작한 SF01-SF05를 격리 브랜치에서 검토하고 통합했다.
- 고정 순서 SF03 -> SF05 -> SF04 -> SF02 -> SF01을 사용했다.
- SF06 자체 production 변경은 없으며 이 요약과 최신 performance evidence만 소유한다.
- reconnectable SSH relay, wire-format 변경, migration, schema 변경은 포함하지 않는다.

## 3. Changes

- SF01: replay 1,024개, Git-label cache 256개 상한과 overflow resync를 추가하고 hidden-active 상태를 cap 전에 적용한다.
- SF02: fresh+retry 합계 256개 상한, Done 우선, retry attempt 보존, bounded in-flight key 추적을 추가한다.
- SF03: archived restore 검증과 persistence rebind가 성공한 뒤에만 runtime archive marker를 커밋한다.
- SF04: 30초 connect timeout, 45초 liveness deadline, partial-frame-aware plain/TLS reader polling을 추가한다.
- SF05: log/archive scan에 4,096 aggregate entry 상한과 mutation 전 two-phase discovery를 추가한다.
- 독립 리뷰에서 발견한 SF01 hidden-active state loss, SF02 committed/in-flight duplicate races, SF03 stale marker, SF04 partial-frame sleep 문제를 각 owner branch에서 수정하고 재리뷰했다.

## 4. Tests

### Focused integration tests

| Command | Result |
|---|---|
| `cargo test -p deppy-sijo coalesce_mux_updated --locked -- --test-threads=1` | Pass: 2 passed, 0 failed |
| `cargo test -p deppy-sijo workspace_git_label --locked -- --test-threads=1` | Pass: 1 passed, 0 failed |
| `cargo test -p web-remote --locked push -- --test-threads=1` | Pass: 42 passed, 0 failed |
| `cargo test -p persist --locked load_workspace_restore -- --test-threads=1` | Pass: 1 passed, 0 failed |
| `cargo test -p runtime --locked restore -- --test-threads=1` | Pass: 8 passed, 0 failed |
| `cargo test -p runtime --locked remote -- --test-threads=1` | Pass: 60 passed, 0 failed |
| `cargo test -p storage --locked logs -- --test-threads=1` | Pass: 25 passed, 0 failed |
| `cargo test -p storage --locked scrollback_archive -- --test-threads=1` | Pass: 11 passed, 0 failed |

### Deterministic gates

| Command | Result | Notes |
|---|---|---|
| `cargo check --workspace --all-targets --locked` | Pass | Final integration HEAD rerun |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | Fail | Pre-existing `crates/terminal/src/alacritty_backend.rs:993` `clippy::manual_repeat_n`; same terminal command fails on `main` |
| `cargo clippy -p deppy-sijo -p web-remote -p persist -p runtime -p storage --all-targets --locked -- -D warnings` | Pass | All changed packages |
| `cargo run -p xtask --locked -- check-deps` | Pass | Exit 0 |
| `cargo run -p xtask --locked -- check-boundary` | Pass | Exit 0 |
| `cargo run -p xtask --locked -- perf-smoke` | Pass | Exit 0 |
| `cargo run -p xtask --locked -- bg01-deterministic-gate` | Fail | Existing repo-wide `cargo fmt --all -- --check` drift; main fails the same format baseline |
| `git diff --check` | Pass | Final integration HEAD rerun |

The exact SF06 deterministic gate is therefore **not approved**. Changed-package validation is green but does not replace the frozen workspace commands.

### Release evidence

- `scripts/render-bench.sh build`: Not run.
- `USE_HARNESS=1 SECS=1800 SAMPLE_INTERVAL=60 scripts/render-bench.sh run wgpu switch 5`: Not run.
- Reason: an existing user-owned debug `deppy-sijo` instance was active; the repository benchmark preflight intentionally rejects concurrent instances to avoid lock, CPU, and measurement contamination. The process was not terminated.
- RSS, child RSS, threads, fd/socket counts, pending replay count, push job count, and post-shutdown return-to-baseline: Pending.

## 5. Acceptance Criteria Check

- [x] Five implementation PRs started from the same frozen base with disjoint production ownership.
- [x] Two unbounded queues and two filesystem/process-lifetime retention paths now have explicit caps/deadlines.
- [x] Archived restore does not consume its runtime marker before validation and rebind success.
- [x] Focused integration tests and changed-package strict Clippy pass.
- [ ] Exact workspace strict Clippy passes.
- [ ] Exact BG01 deterministic gate passes.
- [ ] Thirty-minute release hidden/warm lifecycle measurement is complete.
- [ ] SF06 is eligible to start SSH00.

## 6. Regression Risks

- Replay overflow intentionally requires a later full snapshot resync; focused tests cover cap and hidden-state correctness, not a 30-minute GUI lifecycle run.
- Push delivery is bounded and race-tested, but real network-provider latency and long slow-consumer behavior remain unmeasured.
- Remote liveness surfaces failure after a bounded deadline but does not reconnect or preserve remote PTYs.
- Scan entry-limit failures are fail-closed and non-mutating, so extremely large log directories may require operator cleanup before GC can resume.

## 7. Resource Impact

- Warm/hidden replay: at most 1,024 retained events per workspace.
- Git-label cache: at most 256 paths.
- Web-push jobs: at most 256 fresh+retry jobs plus bounded in-flight status keys.
- Session log and archive scans: at most 4,096 entries per scan.
- Remote client: 30-second connect and 45-second silent-peer deadlines.
- Physical RSS, child RSS, thread, fd, socket, and queue slopes remain Pending because the release soak was not run.

## 8. Security Impact

- No new secret, token, raw payload, external bind, SSH authority, or persistence schema surface was added.
- Existing TLS/token/loopback policy remains intact.
- SSH relay authority remains explicitly outside this wave.

## 9. I18n/CJK Impact

- No locale catalog, user-facing translation, IME, CJK path, or terminal text rendering behavior changed.

## 10. Rollback Plan

- Revert SF01, SF02, SF03, SF04, or SF05 independently using their owner-branch commits and summaries.
- No schema, migration, wire-format, or disk-format rollback is required.
- Do not merge `codex/sf06-integration-evidence` into `main` while the frozen deterministic gate is red.

## 11. Follow-up

1. Resolve the pre-existing workspace format and terminal all-target Clippy baseline in a separately owned maintenance change.
2. Rerun every exact deterministic command without waivers.
3. Stop all other Deppy instances and run the existing 30-minute release benchmark procedure, then record all observable resource slopes; unavailable internal queue metrics remain Pending until an existing approved measurement surface exposes them.
4. Start design-only SSH00 only after deterministic SF06 approval.
