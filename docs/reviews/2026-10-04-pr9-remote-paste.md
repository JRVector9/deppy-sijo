# PR9 — Explicit remote paste

## Objective and source

Add bounded `paste_text` to the visible, explicitly shared MCP sessions using the existing Composer plan and tracked atomic runtime input. Preserve legacy send_text/Ctrl+C/notify contracts and original permissions/authentication/deadline throughout asynchronous durable claims and actual PTY admission.

- Worktree: `/private/tmp/deppy-audit-pr9-20261004`; branch `fix/audit-pr9-remote-paste-20261004`.
- Baseline: `38ee3bece2d0369eddd763d79cd1cbcfe2f2e5b6` (integrated PR1/2/3/4/6/7/8 and Fleet glue).
- Root owns global handoff, combined review/tests/version/build. No native application launch, stop, restart, dev-run, user PTY/clipboard/files, tunnels, version/lock edits, push or subdelegation.
- Workstep/TDD guidance applied. Workstep.md is absent; scoped Korean commit/report is delegated by root.

## Executed RED

- Tool discovery regression: `pr9_explicit_paste_is_discovered_with_no_implicit_submit` failed because paste_text was absent. Gate exit101; `/tmp/deppy-pr9-red-discovery-20261004.log`.
- Cloud actor regression: `pr9_explicit_short_paste_claims_before_dispatch_and_is_idempotent` failed at “valid paste must remain pending until claim completion” because the parser rejected paste_text before asynchronous claim. Gate exit101; `/tmp/deppy-pr9-red-cloud-20261004.log`.
- Runtime queue regression initially returned `Ok(())` after live DEC2004 changed from on to off, while the expected whole-batch result was `AdmissionDenied`. Gate exit101; `/tmp/deppy-pr9-red-runtime-mode-20261004.log`. Final named test is `pr9_actual_paste_admission_rechecks_dec2004_at_queue_boundary`.
- All RED commands used `python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch` with offline/locked package test, `--nocapture --test-threads=1`; compile and execution were held under the source-clean shared gate. Discovery/App parser ran before feature implementation. Runtime's local requirement field/API was scaffolding for the runtime RED; the actual enforcement was absent until that failing behavior was observed.

## Completed implementation and interfaces

- `paste_text` is discoverable and dispatched by the actual MCP server; submit defaults false. Raw text is nonempty and at most32KiB UTF-8. LF, CRLF, tab, Hangul and emoji are preserved through the shared Composer plan. ESC, all other controls and lone CR are rejected before durable claim. Serialized arguments retain the existing64KiB actor bound; complete HTTP envelopes retain the existing64KiB bound. JSON escaping can reject before the raw text cap.
- Cloud `Target` freezes `provider`, `execution`, `known_ai`, `bracketed_paste` with its existing exact UUID/generation/workspace/runtime/session/pane. `list_sessions` exposes `paste_bracketed`, `paste_text_max_bytes`, `paste_ai_confirmed`; they are advisory snapshots, rechecked before input.
- App uses existing `plan_composer_input(...).into_parts()`. It never duplicates encoding. New `Effect::PasteInput {operation_id,parts,admission}` reaches `send_guarded_input_batch`. The old `Effect::Input` and send_text8KiB/no-control/Ctrl+C/manual-shell/dialog contracts remain in place.
- Multiline and tab require verified DEC2004; one-line manual paste remains allowed when mode is off. Codex mode-off never forces bracket framing in the remote path. A known AI without frozen execution evidence is rejected. Durable claim completion rechecks the original projection, grant incarnation/input epoch, server Auth Arc/token/access key and deadline. No fresh local permit replaces the original grant.
- AI admission uses `ExplicitPrompt` for submit=true and `ExplicitAppend` for intentional no-submit append. Draft/dialog guards retain PR2 semantics. Frozen execution `is_current()` runs before Auth admission and again inside Auth's accepted callback, after possible lock delay. Original grant permit and Auth remain held through actual queue retention.
- New local-only `InputAdmission::with_bracketed_paste_required()` is checked against actual `Session.bracketed_paste()` inside the authorization callback immediately before `write_input_batch`. Runtime wire remains22; no serialized requirement was added.
- Pending App operations remain16 /512KiB; paste encoded bytes are included in the original retention accounting. Durable exact-tool/args operation fingerprints, unknown tombstones, finish-before-reply and finish-before-answer-notice are preserved. Unknown is never blindly retried.
- Notify remains own-answer storage and original-session notification, never stdin. No provider turn-wait API was invented; `read_output` remains explicitly stale/cache-based/nonlossless, admission remains distinct from execution/completion.
- Added cfg(test)-only `AgentExecutionIdentity::fixture_current(kind,pid)` wrapping actual capture for private process fixtures, approved by root. It changes no production capture.

Modified product files: `crates/agent-mcp/src/{lib,server,tests}.rs`, `crates/app/src/{app,cloud_agent,agent_detect}.rs`, `crates/runtime/src/{input_admission,in_process}.rs`. Documentation: actual existing `docs/cloud-agent-mcp.md`, linked new `docs/cloud-agent-mcp-guide.md`, this report. Composer/lifecycle implementation, Cargo version/lock and global handoff unchanged.

## Final executed GREEN

Final source-clean gate exited0. Log: `/tmp/deppy-pr9-final-gates-20261004.log`. Gate holds the shared lock through compilation and test execution and cleans workspace package artifacts after a source-worktree switch. Its actual named lists identify MCP3 / runtime1 / App7 PR9 regressions.

| Gate | Actual result |
| --- | --- |
| Full agent-mcp |30passed,0failed,1ignored;0doc tests;0.80s |
| Runtime PR9 mode boundary |1passed,0failed;0.06s |
| Runtime PR1 atomic admission |3passed,0failed;0.13s |
| Runtime PR2 draft/dialog/execution |3passed,0failed;0.13s |
| PTY PR1 actual pressure/batch ordering |4passed,0failed;0.01s |
| App PR9 |7passed,0failed;0.44s |
| All cloud_agent |54passed,0failed,3ignored;12.17s |
| Agent-mcp/runtime all-target Clippy |`-D warnings`,exit0 |
| App all-target Clippy |`-D warnings -A dead_code`,exit0; explicit inherited App dead-code allowance, not a claim of a no-allowance whole-App gate |
| cargo fmt / git diff --check |exit0 |

The actual HTTP/private PTY test uses noninteractive `/bin/sh` startup (`stty`, mode enable, exec `/bin/cat`) and actual captured process birth/group, no AI. It proves Unicode/CRLF/tab output, no implicit Enter, exact-ID retry without redispatch, existing-draft new-submit denial vs intentional append acceptance, and own final notify to original session despite another selection. Then it toggles **actual runtime mode off while claim is pending**, deliberately leaves App's mode=true projection stale, and observes `pty_AdmissionDenied` without marker text reaching cat. Other fixtures prove process exit during claim with unchanged stale execution projection, mode/provider/execution/fallback/revoke/reenable/expiry/rotation/pane changes, unknown exact retries, unsafe/oversize rejection, and durable own answer projection after tab switch/connection stop. Existing PTY default-policy pressure tests prove whole body/Enter refusal without a partial prefix or CR, and queue ordering; these were actually rerun.

The expanded focused run also passed7 before final gates: `/tmp/deppy-pr9-green-expanded-20261004.log`. Earlier source runs are not substituted for the final evidence above. Ignored tunnel/benchmark tests were not run or claimed passed.

Exact final command, workdir `/private/tmp/deppy-audit-pr9-20261004`:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","agent-mcp","pr9_","--","--list"],["test","--offline","--locked","-p","agent-mcp","--","--test-threads=1"],["test","--offline","--locked","-p","runtime","pr9_","--","--list"],["test","--offline","--locked","-p","runtime","pr9_","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","runtime","pr1_","--","--test-threads=1"],["test","--offline","--locked","-p","runtime","pr2_","--","--test-threads=1"],["test","--offline","--locked","-p","pty","pr1_","--","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr9_","--","--list"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr9_","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","cloud_agent","--","--test-threads=1"],["clippy","--offline","--locked","-p","agent-mcp","-p","runtime","--all-targets","--","-D","warnings"],["clippy","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","--all-targets","--","-D","warnings","-A","dead_code"],["fmt","--all","--","--check"]]' > /tmp/deppy-pr9-final-gates-20261004.log 2>&1
git diff --check
```

## Failed fixture approaches and source review

Two initial MCP HTTP budget fixture runs failed in the reader, first on ConnectionReset and then empty response, because an oversized body was still sent after the server refused the declared length. This was a fixture transport issue, not input dispatch. The final actual HTTP fixture advertises its too-large content length and sends no body: the server returns413 before reading/admitting any body. Logs: `/tmp/deppy-pr9-green-initial-20261004.log`, `/tmp/deppy-pr9-green-second-20261004.log`; superseded by GREEN above. The actual JSON args overflow fixture separately rejects the escaped body before any history claim.

Self source review checked original grant/Auth lifetime, both execution checks around Auth lock delay, live mode guard position, shared encoder/no forced Codex mode-off, whole tracked admission, argument/retention accounting, legacy callback behavior, exact durable fingerprints/unknown receipts and original notify. No confirmed remaining PR9 defect found. Root explicitly owns the independent CLI source review, combined full suites and release gates; this report does not claim those root gates ran.

## Integration handoff

Scoped PR9 implementation is complete and source frozen after final gates. Root must apply the scoped Korean commit and preserve PR5 Composer/lifecycle seams. The only app.rs changes are cloud projection/new effect admission. Global handoff/version/release build/package verification remain root-owned. No artifact was produced or delivered under0.5.5.

Exact next commands: `git status --short`; `git log -1 --oneline`; `git diff 38ee3bece2d0369eddd763d79cd1cbcfe2f2e5b6..HEAD --stat`; inspect new `Effect::PasteInput` and local `with_bracketed_paste_required`, then integrated source review/tests. Rerun the gated named/focused command above only if integration changes warrant it. Never run scripts/dev-run.sh or launch/stop/restart Deppy without fresh explicit authorization.
