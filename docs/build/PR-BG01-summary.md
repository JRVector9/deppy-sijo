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
