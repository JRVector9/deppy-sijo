# PR7 — Cloud durable history worker

## Scope and source

- Baseline: `ed2c9163b21516b704234d5c20360dfa80d2d2bc` (integrated PR1/3/6, inherited version 0.5.5).
- Isolated worktree: `/private/tmp/deppy-audit-pr7-wave2-coherent-20261004`; branch `fix/audit-pr7-cloud-history-worker-wave2-coherent-20261004`.
- Files: `crates/app/src/cloud_agent.rs`, new `crates/app/src/cloud_history_worker.rs`, App cloud pump/constructor in `app.rs`, module registration in `main.rs`, bounded history access in `crates/agent-mcp/src/history.rs`, this report.
- No native Deppy launch, stop, restart, dev-run, user PTY input, user database, user credentials, version/lock edit, push, delegation, or shared root handoff edit. Runtime tests use isolated fixture PTYs already present in the cloud suite.

## Architecture and invariants

`CloudAgent::new(path, redaction, egui_context)` is inert. First App `poll_history(send)` admits database initialization. The actor opens/migrates its own SQLite connection and exclusively performs open, claim, finish and recent reads. The worker holds no runtime handle, terminal input callback, or agent launcher. Completion publication precedes its egui wake. An early connection Start waits for initialization; failures stop that attempt rather than repeatedly reopening on every frame.

`handle(req, send)` validates the unchanged tools/arguments and prepares a bounded payload. Read/list requests remain App-owned. Effectful requests enqueue a durable claim and return without SQLite or terminal dispatch. The claim completion returns to App before any input effect. App captures and checks the original session UUID/generation, workspace, runtime instance, worker session, pane, live state, read-grant incarnation, input-grant epoch, original permission lease, server Auth Arc, token epoch/OAuth access key and request deadline. Permission revoke→reenable and unshare→reshare cannot make a pending old claim valid.

The existing `InputAdmission` keeps that original input permit and frozen authentication/deadline authorizer through actual PTY queue admission. The App sender still checks the original runtime/pane projection immediately before runtime command admission. Routing changes or ended targets revoke the old input permit. PR2/PR9 will additionally compose the supported AI execution/draft/dialog guard using their approved interfaces; this PR preserves the legacy 8 KiB/control rejection contract.

Only a successfully committed New claim may dispatch input. A duplicate Existing receipt never dispatches another effect. The original durable unknown/retry=false tombstone survives process/worker shutdown and finish failure. A correlated `InputAdmitted` event only enqueues finish; `AdmissionUnknown` stays unknown with `pty_admission_unknown`, retry=false and completion not confirmed. Successful PTY queue admission is not represented as AI execution/completion.

Notify never enters terminal stdin. It is claimed and finished before the truthful stored response or original-session `AnswerNotice` is delivered. Completion retains its original target/title even if another runtime/session is selected. Records/answers update through one coalesced list request. A list issued before a newer completed finish is discarded and refreshed, and monotonically checked revisions prevent an older snapshot replacing a newer one. The original session ID remains the answer's identity; stopping the listener does not stop history completions or answer projection.

When MCP/settings are inactive and no claim needs routing validation, App retains the existing target-projection fast path: it polls the actor and projects retained answers without rebuilding 256 target/title entries per frame. Actor waits and database I/O never run in App render/interactive logic. Only explicit App shutdown may wait: it revokes input first, cancels pending App claims/admissions, drains already accepted finish jobs and joins the DB worker. A pending claim can leave only a durable unknown tombstone; the DB actor cannot type after shutdown/revoke.

## Budgets

| Resource | Bound |
|---|---|
| App effectful operations across claim/PTY/finish phases | 16 |
| Pending App prepared input/answer logical bytes | 512 KiB |
| Actor outstanding jobs + retained completions | 32 total |
| Actor charged job/result logical bytes | 4 MiB total |
| Claim argument JSON | 64 KiB; identity strings separately capped |
| Receipt JSON | 2 KiB |
| One coalesced recent snapshot | 3 MiB of row text; up to 500 latest audit + 100 retained answer rows |
| Each answer body | unchanged 16 KiB; unchanged latest 100 completion retention |
| Durable operation tombstones | unchanged 100,000 cap, never evicted to permit replay |

The actor reserves a result allowance with each job and releases it only when its completion is consumed. A second unconsumed 3 MiB snapshot cannot bypass the 4 MiB budget. Claims are moved into the actor; App does not retain a second full argument JSON. Known-unsent jobs return ownership and are never silently replaced. Finish/list work is coalesced/bounded by the same admitted operation count; only finishes with preserved unknown tombstones may wait for capacity.

These are logical payload/count limits, not allocator or RSS measurements. String/Vec capacity, JSON/container/channel metadata, SQLite page cache, one bounded temporary row/serialization buffer, record-to-answer view copies and thread stack add bounded overhead. The retained App records/answer projection is separate from the actor's completion reservation (at most the stated snapshot plus 100 answer bodies). No process/GPU memory plateau claim is made.

History queries CASE-check byte lengths before SQLite materializes fingerprint/outcome/message strings. Oversized at-rest fingerprint/outcome values fail closed and remain untouched. Existing real claims require exact SHA256 equality; bounded legacy fixture fingerprints still fail by ordinary fingerprint conflict. `finish` also caps outcome JSON, and `recent_bounded` checks total row bytes. These guards do not prune or change durable input tombstones.

## Observed RED and GREEN

The first test-only source was compiled and executed through the corrected Cargo gate before implementation. Both tests failed for the intended product behavior:

- `pr7_locked_sqlite_claim_does_not_block_interactive_handle`: exclusive SQLite lock blocked the interactive call for **129.735167 ms**, exceeding the desired 20 ms fixture gate. The original audit measured 127.305 ms separately; that is not this test's number.
- `pr7_claim_completion_is_required_before_input_dispatch`: the old synchronous handler dispatched **1** input effect before returning, whereas the asynchronous contract required **0** before App consumes claim completion.

Log: `/tmp/deppy-pr7-red-20261004.log` (0 passed / 2 failed). The final locked fixtures measure the new handle/finish admission plus 100 nonblocking App polls, print exact durations, and assert below 20 ms while durable reply/notice remains unpublished. This measures functions under a real SQLite lock, not actual GUI FPS or a complete user interaction.

Meaningful named fixtures additionally cover: 13 expiry/revocation/exact-target variants during pending claim; durable unknown claim visible through a separate SQLite connection at the effect callback; AdmissionUnknown exact retries; 16-operation/actor byte/count rejection without eviction; stale list completion vs new answer; original target preservation across new runtime; 8 MiB at-rest fingerprint/outcome rejection with original row length unchanged; lazy worker open and publication-before-wake; admitted finish drain on worker Drop; App shutdown cancelling input claims and preserving completed answers. Test helpers drain real completion channels with deadlines, not sleeps/unbounded spins. Existing cloud harnesses use that helper for their synchronous test assertions; production handle/observe calls remain nonblocking.

## Actual verification commands and provenance

Every Cargo command used `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`. The gate holds its flock through compilation and execution and cleans workspace package artifacts on a worktree path change. No ungated shared-target test result is claimed as this PR's proof.

Final source gate (exact command, run from this isolated worktree):

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr7_","--","--list"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr7_","--","--test-threads=1","--nocapture"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","cloud_agent","--","--test-threads=1"],["clippy","--offline","--locked","-q","-p","agent-mcp","--all-targets","--","-D","warnings"],["clippy","--offline","--locked","-q","-p","deppy-sijo","--all-targets","--","-D","warnings","-A","dead_code"]]'
cargo fmt --all -- --check
git diff --check
```

Final log: `/tmp/deppy-pr7-final-source-gates-20261004.log`: exact compiled list of **12 PR7 cases**, **12 passed / 0 failed** in 0.34 s; Cloud filter **47 passed / 3 ignored** in 10.89 s. Locked claim handle + 100 polls measured **42.709 µs**; locked finish admission + 100 polls measured **88.041 µs**. Both stayed below the fixture 20 ms gate while the actual SQLite lock remained held. The actor still performs its bounded DB wait; the measured reduction is removal of that wait from interactive App calls, not a claim that SQLite or a full cloud task completed in microseconds.

The final MCP strict all-target Clippy and App all-target Clippy with the explicitly noted baseline dead-code allowance exited 0. `cargo fmt --all -- --check` and `git diff --check` were executed and exited 0. There is no unqualified strict App pass claimed for this isolated baseline; root's combined gate must remove the baseline PR3 warnings.

Affected MCP tests were actually run on the same product history source in `/tmp/deppy-pr7-final-gates-20261004.log`: `test --offline --locked -q -p agent-mcp -- --test-threads=1` through the same batch wrapper, **27 passed / 1 ignored** in 0.70 s; doc tests 0. Ignored cases include public tunnel/network and benchmark cases, not executed or claimed passed. Existing real OAuth/fixture PTY roundtrip and queued input revoke tests are part of the cloud filter.

Failed intermediate approaches: the first sender extraction accidentally removed an inner closure delimiter (fmt/compile rejected it); a result Option/Result mismatch was then corrected. Strict all-target App/MCP Clippy caught two PR7 lints (boolean simplification and large known-unsent Job error); both were corrected, with error-only boxing retaining exact job ownership. Strict App Clippy also reported integrated baseline PR3 `PromptLibrary::{load,try_upsert,save}` production dead-code warnings. Those files are outside PR7 ownership; root delegated the fix to PR4. Accordingly the isolated App gate uses explicit `-A dead_code` after `-D warnings` and is not described as an unqualified strict App pass. MCP's all-target gate remains strict.

## Integration and remaining work

- Root will integrate the scoped commit against the stated baseline, preserve other App wiring, run the combined independent CLI review/full suite and unqualified strict Clippy, and handle version/package verification. This PR creates no release artifact.
- PR9 uses this App claim/finish state machine for explicit paste/submit and composes PR2's `AgentExecutionIdentity::is_current()`, `input_guard(false)` and `InputAdmission::with_agent_guard` with the existing grant permit/Auth authorizer. It must not replace the grant permit with a fresh local permit.
- No native GUI, user/provider TUI, public Cloudflare, long-running RSS/allocator or Windows/Linux execution measurement was performed in this isolated task.

Next-agent source commands:

```sh
git diff ed2c9163b21516b704234d5c20360dfa80d2d2bc..fix/audit-pr7-cloud-history-worker-wave2-coherent-20261004 --stat
git show --stat fix/audit-pr7-cloud-history-worker-wave2-coherent-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr7_","--","--test-threads=1"],["test","--offline","--locked","-q","-p","agent-mcp","--","--test-threads=1"]]'
```
