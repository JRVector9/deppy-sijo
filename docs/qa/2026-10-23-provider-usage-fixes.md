# Claude / Grok usage corrections and protected harness

Completed 2026-10-23 in `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, branch
`fix/product-review-integration-20261023`, base HEAD `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1`
plus uncommitted working tree. Previous SSH/launcher changes are preserved. No new agent
was delegated, GUI restarted, release artifact delivered, or commit/push performed.

## Corrected behavior

| Area | Verified cause | Implemented correction |
|---|---|---|
| Claude source | A six-hour-old partial file suppressed the live CLI fallback | Refresh after 60s or a missing window; compare original timestamps and merge the newest available value per window |
| Claude expiry | Window lifetime was mistaken for reading freshness | Retain successful measurements for at most 10m; reject future timestamps, apply known reset times and forbid older pre-reset CLI values from restoring expired slots |
| Claude parser | Percent search crossed the next heading and selected model-specific weekly quotas | Stop at section boundaries and select the all-model weekly section |
| Both collectors | Greedy OSC stripping consumed visible text between ST controls | Shared linear scanner ends each OSC at its first BEL/ST and preserves intervening UTF-8 text |
| Grok startup | Current trust wording was unknown; text+Enter bursts did not submit in the installed TUI | Handle old/current wording in the owned probe only; separate text and Enter events by 100ms inside the original 25s budget |
| Snapshot writer | Database signature dedup skipped measurement heartbeat and reset metadata | Refresh the snapshot before DB dedup, preserve optional resets and write atomically; unknown windows stay null |

Rendering geometry and normal provider labels/order are retained. I/O remains on workers.
CLI refresh admission remains at most once per minute and failures retain the original
measurement timestamp. Unknown values display a dash rather than an invented zero.

## Protection

`docs/testing/provider-usage-contract.json` seals eight focused adapter/support files,
verifier/self-tests, and usage-only regions in the app model/view/source selection,
actual status-bar call, snapshot writer, mandatory UI/writer tests, CI and AGENTS policy.
SHA256 changes fail verification. Exact required test names are checked before running;
removed, added without review, duplicated or ignored mandatory cases fail.

The executable harness has **20 App + 3 writer tests, zero ignored**. Five mutation self-tests
exercise whole-file/region changes, missing or duplicated markers, missing files and
removed/ignored mandatory tests. Linux CI fast-checks has the hash/mutation gate; macOS
has the executable harness gate. These CI commands passed locally; no GitHub run or
branch-protection setting is claimed. Protection is an explicit change guard, not an OS
immutable bit or a security boundary against deliberately replacing both guard and manifest.

Usage changes require an explicit user request, a failing regression, a reviewed correction,
an authorized seal and full verification. Unrelated work must not reseal to pass CI.
Procedure: `docs/testing/provider-usage-harness.md`.

## Actual validation

All Cargo commands ran serially with `--offline --locked`, `CARGO_BUILD_JOBS=2` and
`CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`.

| Check | Actual result |
|---|---|
| Full App `cargo test -p deppy-sijo -- --test-threads=4` | 3796 unit + 39 integration = **3835 PASS**, 46 unit + 3 integration ignored; isolated child fixture 1 PASS excluded from the total |
| Full proxy `cargo test -p mcp-proxy -- --test-threads=4` | **112 unit + 9 integration = 121 PASS**, 1 ignored; command 11.640s |
| `python3 scripts/verify-provider-usage.py --offline` | Source hashes PASS, App20 + proxy3 mandatory PASS, **zero ignored** |
| Python guard mutation tests | **5 PASS** |
| Clippy App/proxy `--all-targets -- -D warnings` | PASS, 33.489s |
| `cargo fmt --all -- --check` | PASS, 2.838s |
| `cargo run -p xtask -- check-boundary` | PASS, 4.833s |
| `git diff --check` | PASS |
| Actual installed Grok original ignored live test, usage required | PASS, 9.151s command; same production fetch function |
| Actual installed Grok source-included live observation | Weekly **57% remaining**, 8.870s fetch |
| Actual installed Claude source-included live observation | **6% used / 85% used**, 4.521s fetch |

The source-included live observer calls the original fetch/parser, with only unused UI
plumbing stubbed. It stores quota results and fixed phrase counts, never raw terminal or
credential output. The actual egui harness checks visibility/order/partial values at
300/430/620/810/1200pt. No running GUI end-to-end/restart, public release or future CLI
version guarantee is claimed. External account probes remain opt-in and excluded from CI.

## Failed approaches retained

Initial collector regressions: 1PASS/6FAIL; source arbitration: 11PASS/6FAIL. After those
fixes the focused usage suite was 77PASS/5ignored, but the real Grok probe still failed
at 25.31s. An explicit y+Enter burst also failed. A per-key control succeeded at 9.397s;
a narrower text/Enter split in a fresh owned directory succeeded at 9.056s. The original
fake PTY accepted a combined burst too generously. Tightening it reproduced 1PASS/3FAIL;
the corrected input path then passed all20 App harness cases. Snapshot writer tests were
0PASS/2FAIL before the heartbeat/reset correction, then passed. An initial no-op change
guard failed all5 mutation cases; the actual verifier passed them. No failing assertion
was removed or ignored. One apply_patch context check failed atomically without edits.

Evidence: `docs/qa/assets/provider-usage-fixes-20261023/`, including RED/GREEN logs,
redacted live results, exact protected source hashes and a SHA256 evidence index. Closed
logs are losslessly compressed. Source/test version remains0.18.0 because no changed
artifact is being released. A later delivered changed bundle must increase beyond0.18.0
and verify app/bundle versions. Running GUI was not restarted.
