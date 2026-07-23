# PR-BG01 summary

## Scope

This app-independent slice closes render-host edges in the owned Agents, Environment, Home, and
structured Agent Sessions leaves. It adds no allowlist, runtime command, worker, timer, poller,
network client, or dependency.

## Boundary changes

- `AgentsUi` and `EnvProfilesUi` retain their immutable snapshot / at-most-one-intent contracts.
  Their production-source regressions now also reject filesystem, process, network, clipboard,
  picker, and delayed-repaint edges.
- Home renders the cached notice snapshot only. A 300-frame regression emits no host platform
  command, and a source regression rejects filesystem/process/network/picker/keyring/periodic
  repaint edges. Translation CLI discovery is outside this leaf and must be supplied as a cached
  root snapshot.
- `AgentSessionsUi::show` no longer synchronizes process configuration or executes controller
  actions. It returns one opaque, non-Clone/non-Serialize/non-Debug `AgentSessionsDeferredAction`.
  Only the first action is retained, a newer frame generation rejects stale work, and the root
  executes it on a later logic tick through `execute_deferred`.
- The leaf no longer has a fallback `CodexAppServerClient::spawn` path. Production construction is
  possible only through the root-owned `CodexAppServerHost` port.
- Concrete `storage::StructuredThreadRow` was replaced by `AgentSessionPersistedRow`. The UI
  projection is capped at 500 rows, 32 KiB per row, and 4 MiB aggregate. Prompt/follow-up/steer
  input is capped at 1 MiB and workspace cwd at 32 KiB with NUL rejection.

## Required root integration

- Retain at most one `AgentSessionsDeferredAction` in `App`; execute it at the next logic tick,
  feed its optional `AgentSessionsRequest` to the existing handler, and then call
  `sync_controller_config` outside render.
- Map each concrete structured-thread storage row field-for-field into
  `AgentSessionPersistedRow` before `import_persisted_threads`.
- Add an xtask source gate for these four leaves using the same forbidden host/dependency patterns
  as their test-only source regressions.

## Remaining unowned blockers

- `ui/inbox_waiting.rs` starts log-tail filesystem/thread work from its UI call graph.
- `ui/diff_panel.rs` performs production metadata access.
- `ui/workspace.rs` and `ui/file_tree.rs` still require their separate intent/host lane closeout.
- Home translation CLI availability, root cwd validation, and cold activity-name filesystem probes
  live outside this slice and must become event-driven snapshots.
- One-shot deadline repaint requests may remain, but periodic status/activity polling repaint is not
  permitted by the final gate.

## Verification

- Direct rustfmt `--check` for all four owned Rust files passes.
- Scoped `git diff --check` passes.
- The production-prefix scan finds no direct storage/filesystem/process/network/picker/keyring,
  delayed-repaint, or concrete app-server spawn edge in the owned leaves.
- The first focused Cargo command did not select tests because compilation stopped at the expected
  shared-tree integration seam: `app.rs` had not yet mapped `StructuredThreadRow` to the new DTO.
  Concurrent Workspace/FileTree and web-remote lanes also had mid-edit API/test mismatches. This is
  not accepted as test evidence; focused tests, app check, and strict Clippy remain for root after
  all three APIs are integrated.

## Integrated closeout update

The root integration listed above is complete. Workspace, FileTree, Diff, Inbox, approval,
Activity, Notifications, Agent Sessions, dotenv, and status-feed projections are wired through
bounded snapshot/intent or host-adapter paths. Composer prompt submission and Connector tool-page
requests are staged for the next logic tick, so render starts neither runtime writes nor Connector
worker/database lifecycle.

Production `workspace.rs` now contains zero `RuntimeClient`, `RuntimeCommandSink`, `send_command`,
or native-notification effect. Queue plus in-flight protocol work has one hard capacity of eight;
local terminal input is capped at its existing 1-MiB clipboard ceiling, search at 32 KiB/1,000
matches, pane/tab IDs at 128 bytes, split paths at 256 items, and scrollback at 100,000 lines.
Exact operation/generation completion releases capacity on success or sanitized delivery failure;
stale/duplicate completion does not.

The current integrated evidence is Workspace 55/55, status-feed 17/17, notice translation 11/11,
and xtask 9/9, plus app all-target check, strict app/xtask Clippy, full rustfmt, diff-check,
zero-allowlist boundary, and the clean 23-crate dependency DAG. The earlier pre-integration test
paragraph remains as historical failed-attempt evidence and is not counted as a pass.

Remaining development before final BG01 hardware approval is tracked in the handoff: bounded
app-server queues, bounded Git/local-LLM/Tailscale output, clipboard-cache limits, and latest-only
agent-detection delivery. The 30-minute and Scenario A-E measurements remain deferred until those
development changes freeze.

## Second resource-wave update

The bounded app-server, Git/worktree, local-LLM/Tailscale, clipboard-cache, and AppHost file-
operation changes are now integrated and independently verified. The full app suite passes 720
tests with five explicit hardware/external-resource ignores, logging policy passes 15/15, and the
strict app/xtask, zero-allowlist, dependency, fmt, and diff gates are green. The only named
development blocker from the preceding paragraph is now latest-only agent-detection delivery plus
bounded detector process/file inputs; long-duration and Scenario A-E measurements remain deferred
until that code freezes.

## Agent detection/session closeout update

Latest-only agent detection, bounded process/filesystem capture, and bounded retained structured
session state are now integrated. Empty detection input parks without backend I/O or repaint,
stalled UI consumption retains at most one outcome, and production ps/lsof/transcript discovery
has explicit time/item/byte/depth limits with process-group cleanup. Session table rendering borrows
cached rows and no longer clones whole sessions; retained items, approvals, files, nested values,
diagnostics, and total projection bytes are capped and secret-bearing event types are non-Clone.

The full app suite now passes 752 tests with five explicit real-resource ignores and logging policy
passes 15/15. App/xtask all-target check, strict Clippy `-D warnings`, xtask 9/9, zero-allowlist
boundary, the clean 23-crate dependency DAG, full rustfmt, and diff-check are green. Remaining
development before hardware approval is limited to the separately recorded startup/action input
seams and corrupted persisted-session admission; 30-minute and Scenario A-E measurements have not
started.

## Dotenv secret fail-closed preflight

The bounded dotenv apply phase now acquires rotating redaction leases for the complete secret set
before creating a profile or mutating persistence/keyring. Redaction, rotation, creation, and
physical-slot resolution failures return static hard errors; they cannot be skipped while the
overall sync reports success. Post-retirement cleanup remains recoverable only through the durable
exact orphan ledger. Exact single-secret and second-secret-overflow regressions prove zero profile,
credential, or keyring mutation when corpus preflight is incomplete. The integrated app suite
passes 792/792 with five explicit ignores, logging policy 15/15, and storage 189/189 plus doc tests.
App/storage all-target check, strict Clippy, zero-allowlist boundary, the clean 23-crate dependency
DAG, full workspace fmt, diff-check, and the complete secret-like persistence/log scan pass.

The eager worker, two-second fallback, 25-ms retry, five-second empty-env restore, and direct launch
admission are still active development tracked in the handoff; this fail-closed change is not a
claim that the complete dotenv lifecycle gate is finished.

## Lazy AgentState worker foundation

The DB-neutral AgentState worker protocol is now frozen before the atomic app cutover. Construction
creates no thread or channel; the first admitted aggregate creates one capacity-one worker and one
backend, projections coalesce independently by section/revision, and at most eight exact
continuations retaining 4 MiB are kept FIFO. Exact kinds cover generation-aware turn clear,
identity-CAS binding delete, and a 16-item/512-KiB structured batch. One backend call returns one
Arc-shared complete snapshot or one static failure for the exact operation and every same-job
projection.

Idle resources exit after 30 seconds through a lifecycle-locked final receive. Explicit
`shutdown_drain` stops admission, discards coalescible projections, and settles the in-flight then
queued exact work in FIFO order; unknown delivery returns one `WorkerUnavailable` completion and
is never retried. Focused tests pass 13/13, app all-target check passes, and strict Clippy passes
with only the expected pre-wiring dead-code lint excluded. Scoped rustfmt, diff, and forbidden-edge
checks pass. Literal strict Clippy and removal of the legacy direct app path remain atomic cutover
gates; this foundation is not a production dual-path completion claim.

The shutdown durability amendment adds authoritative `BindingReconcile { items }` exact work.
Its item bound matches storage's maximum of 256 live/desired rows, its retained payload is capped
at 4 MiB, and an empty reconcile is meaningful because it removes all stale bindings. Normal
`BindingSync` remains latest-only. Shutdown drains reconcile work in FIFO order; an operation
whose delivery is unknown is never retried, while work proven unsent may start one fresh worker
for the entire bounded drain before remaining items settle as `WorkerUnavailable`. A repeated
race regression exposed and closed the original channel-disconnect/handle-exit gap. Focused worker
tests pass 13/13, including ten repeated unknown-delivery runs; check, dead-code-exempt strict
Clippy, scoped fmt, source-law, and diff-check pass. App wiring must stage the current reconcile
immediately before `shutdown_drain` and report every failed completion.

## Lazy dotenv worker foundation

The app-independent dotenv execution primitive now constructs without a thread, channel, I/O
resource, timer, or repaint. Its first accepted operation starts one standard worker, opens the
caller resource only after dequeue, and reuses it until a lifecycle-locked 30-second idle exit.
Pending state is latest-one; exact continuations are FIFO and capped at eight. Duplicate
outstanding operation IDs are rejected, channels retain one wake/result, stale generation and
revision outcomes are explicit, and the completion wake runs only after result publication.

Drop closes and joins the worker; factory, execution, panic, thread-spawn, duplicate, capacity, and
stale failures are static low-cardinality codes. Focused/full dotenv tests pass 11/11 and 39/39,
app all-target check passes, and strict Clippy passes with only expected pre-wiring dead code
excluded. Scoped rustfmt and diff-check pass. The eager app worker, timer fallbacks, and ungated
launch paths remain the atomic root cutover and literal strict-Clippy gate; no production dual path
is claimed by this foundation.

## Workspace project-name render boundary

`WorkspaceUi` no longer calls project-name discovery or filesystem APIs from title rendering.
It consumes a revisioned immutable `(SessionId, cwd) -> display name` projection computed by the
App host, bounded to 256 entries, 1 KiB per name, and 4 MiB retained text. Lookup is a binary
search over one `Arc<[Entry]>`; clones and same-revision installs reuse that allocation, and cwd
or mux-liveness changes prune stale labels before they can be rendered. Debug output contains only
entry and byte counts.

Root independently reran all 58 workspace tests and diff-check. Fake-snapshot exact/stale/bounds
tests, the production-prefix filesystem source law, default all-target check, dead-code-exempt
strict Clippy, full fmt, and the manual forbidden scan pass. Optional all-features validation is
environment-blocked because `libghostty-vt-sys` attempts a GitHub DNS fetch; no source workaround
was added. Root still must compute and install the projection off-render, incrementing its revision
on cwd or name-style changes.

## Authoritative structured-session catalog

`AgentSessionsUi::replace_persisted_threads` accepts only a complete multi-workspace snapshot. It
prevalidates the existing 500-row, 32 KiB/row, and 4 MiB total ceilings, checked byte arithmetic,
local-session identity, and duplicate local/thread IDs before mutating UI state. Persisted-only
placeholders have explicit ownership and are removed when omitted by a complete catalog; the first
runtime request or event promotes a placeholder, so attached, pending, running, and event-bearing
sessions survive replacement even if their persisted row disappears. Rejected input leaves rows,
byte counts, sessions, selection, and placeholder ownership unchanged. Row Debug output contains
only field lengths/flags and rejection codes contain no row data.

Root independently reran all 41 AgentSessions tests and diff-check. Exact replacement/removal,
runtime-state preservation, overflow/duplicate/invalid rollback, and hostile Debug regressions
pass, as do all-target check, dead-code-exempt strict Clippy, and targeted fmt. One test initially
needed an explicit `sum::<usize>()`; a concurrent app API mismatch transiently blocked a full run,
which passed after root stabilized the shared tree. Root must now apply this API only for a current
complete Catalog completion; partial or stale pages may never replace the catalog.

## Bounded AgentState production cutover

Agent hook/status, attention, persisted PTY binding, structured-session catalog, activity catalog,
project-name, and resume-transcript state now flow through one lazy App-owned AgentState worker.
Construction opens no database and creates no thread; the first real request opens one worker-owned
connection. UI code consumes immutable snapshots and emits bounded mutations. Resume transcript
inspection is off-thread, globally single-flight, item/byte/depth bounded, and returns only sanitized
results. The production source-law test rejects the removed direct storage receivers, synchronous
transcript probes, cached project-name path, and legacy persisted-thread import path.

Exact work remains FIFO 8 with an aggregate 4 MiB ceiling. The UI retains at most one coalesced
mutation per 500 persisted sessions, 32 KiB per row, and 4 MiB total. App prepares the largest
actual-retained-byte prefix that fits the existing 16-item/512-KiB worker request ceiling, including
Vec capacity, enum storage, String capacities, and the request wrapper. A maximum-size valid
16-row UI batch therefore splits instead of being rejected; only a successfully staged prefix is
removed. Shutdown quiesces producers, drains old scope, then drains at most 65 structured waves in
the worst one-item-prefix case before one final binding/turn reconcile. Unknown delivery is reported
once and never retried.

Storage exposes independent hook/status, attention, binding, structured, and activity projection
flags while preserving the legacy complete-projection constructor defaults. App starts every flag
false and opts in only for the requested section; exact-only work performs no projection query.
Omitted sections skip epoch lookup where possible, SQL preflight/select, parameter buffers, and
output allocation. Selected projection validation, exact mutation, actual retained-byte validation,
and commit remain in one IMMEDIATE transaction, so any selected-section failure rolls all co-staged
mutations back and no fallible operation follows commit.

Verification passes: app all-target check and literal strict Clippy; worker 22/22; transcript and
detector 34/34 with one real-process smoke ignored; AgentSessions UI 47/47; structured-prefix 2/2;
AgentState boundary 4/4; storage AgentState 25/25 and full storage 214/214; zero-allowlist boundary;
the clean 23-crate dependency graph; full rustfmt and diff-check; and the complete security scan,
including audit 52/52, MCP 103/103, and proxy 53/53 with one env-gated soak ignored. Three independent
read-only audits found no remaining loss, shutdown, scope, selective-query, transaction, or retained-
accounting blocker. Wall-clock 30-minute and Scenario A-E hardware measurements remain deliberately
deferred until CR01/BG01 structural development is complete.

The root full app run passed 841 executable unit tests with five explicit real-resource ignores;
the only four unit failures were the checkout's previously recorded managed-sandbox bind denial in
three approval Unix-datagram cases and one loopback LLM-proxy case. All four fail at resource bind
before product behavior. The run also exposed a stale committed dotenv source-law sentinel that
still named the deleted synchronous resume helper. It now inspects `apply_resume_probe_results` and
requires the completion-time dotenv source-stamp and shell-only guards around the tracked WriteInput
process exception; the corrected integration test passes 5/5 and literal strict Clippy passes again.

## Final structural freeze

The remaining structural resource and release-gate work is integrated. Settings auxiliary workers
and the Home status feed are inert until explicit use, capacity-one/latest-only, joined on shutdown,
and release idle resources after 30 seconds without polling. Background workspace shutdown is
owned by a two-slot registry whose panic-safe completion flag is published before its one wake.
Environment invalidation preserves exact in-flight generations, and status workers publish stopped
state under their lifecycle lock on both idle expiry and pre-first-cycle cancellation. Independent
follow-up review found no remaining event-order or restart race.

Settings writes now perform bounded same-transaction admission, while audit retention keeps 4,096
finalized rows, 8 MiB, and 30 days through one steady-state indexed window. Legacy/aged overflow is
normalized only after the exact typed marker rolls a lifecycle transaction back, in separate
64-row/8-MiB transactions before one DB-only retry. Prepared rows remain durable and external calls
cannot start before a successful preflight. Audit passes 64/64 and storage 233/233 plus doc-tests.

Production packaging is trusted and fail-closed by default. The app and proxy are the only release
build targets; the bundle and both executables must share an Apple-anchored Developer ID Application
team, hardened-runtime flag, and trusted timestamp. Untrusted local packaging requires an explicit
two-variable opt-in. The pending workflow template pins checkout to a full commit SHA and adds no
protected workflow path to the least-privilege push history.

Final integrated deterministic evidence is green for xtask 12/12, zero allowlist, the clean
23-crate DAG, full workspace format, locked all-target check, literal strict Clippy, all v0..v31
migration prefixes, performance smoke 16/16, failure injection 10/10, i18n, and proxy 53/53 with one
explicit environment soak ignored. The serialized workspace run completed every target: 2,051 tests
passed and eight real-resource tests were ignored. Exactly 146 tests failed across auth, app, MCP,
runtime, and web-remote; every failure is the managed sandbox denying a local listener/socket bind
with `EPERM`. The single BG01 entrypoint therefore passes through migration and stops at the same
security-scan bind denial. No bypass was added.

Structural development is frozen. Production approval still requires the unchanged security/full
suite on a host that permits local sockets, the 30-minute idle/discover/cancel slope and release
Scenario A–E hardware measurements, a trusted signed package build, and approved real external
OAuth/account smoke. Those are release-environment gates, not additional architecture work.

## Deterministic gate hardening

The local dependency graph now comes from Cargo metadata rather than hand-parsing direct `path =
"../..."` syntax, so workspace-inherited, target, build, and development dependencies cannot bypass
the forbidden-edge or cycle checks. Leaf and composition-root scans parse complete Rust files and
exclude only explicit test-only top-level items; production items appearing after a test module are
still checked. There remains no boundary allowlist.

`bg01-deterministic-gate` is the single fail-fast entry point for formatting, workspace all-target
check, literal strict Clippy, migration smoke, security scan, exact performance smoke, the complete
failure-injection matrix, i18n, and the serialized full-workspace regression run. The macOS CI
workflow definition is preserved under `docs/build` because the current least-privilege GitHub OAuth
credential cannot update `.github/workflows`; installing it there remains a credentialed BG01 action.
Hardware Scenario A–E, wall-clock
RSS/thread/socket slope, trusted Apple signing, and real external-account smoke deliberately remain
separate release approvals rather than being reported as deterministic CI evidence.

The macOS package path now signs the proxy helper and main executable before the outer bundle.
Developer ID mode requires hardened runtime and a trusted timestamp; production mode rejects every
fallback identity. A separate verifier checks exact plist identity/version/executable fields,
required architectures, strict nested and deep signatures, Developer ID authority and TeamIdentifier,
archive extraction, and binary hashes. The deterministic gate source-checks these steps and runs
shell syntax validation. Building and signing a release artifact remains intentionally deferred to
the final credentialed gate.
