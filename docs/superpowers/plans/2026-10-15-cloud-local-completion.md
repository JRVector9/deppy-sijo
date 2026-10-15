# Cloud Local Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Return a verified local native-agent turn completion to the original authenticated cloud submission when its exact connection/session has opted in.

**Architecture:** Keep the existing 14 tools and ordinary JSON replies. Atomically reserve a bounded completion watch and dedicated SSE transport before durable input claim; pin verified input sequence, execution/native turn identity and reducer evidence. Persist one original-identity receipt before returning the final tool result, with separate local and delivery states.

**Tech Stack:** Rust workspace, existing storage reducer/SQLite actors, runtime postcard protocol, MCP Streamable HTTP, egui and five existing localization files.

---

## Authorization and execution

The user approved the written design with `진행해` on 2026-10-15. The previous request explicitly selected GPT-6.1-Sol/xhigh parallel workers; do not ask for another execution preference. The referenced superpowers execution skills are absent from the available skills/files, so use the native collaboration APIs for the already selected workflow and root specification/code review. No app launch/restart or real-account messages are authorized. Synthetic private loopback and isolated temporary DBs only.

Canonical root: `/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015`, branch `feat/cloud-local-completion-20261015`, base `fb3357d2`. Root is the sole Cargo producer with shared target `/Users/jr/Desktop/projects/deppy-sijo-performance/target` and `CARGO_BUILD_JOBS=2`.

## Ownership and PR units

1. **PR1 native proof/input binding:** proof worker owns `crates/storage/src/agent_attention.rs`, narrow snapshot/export changes in storage `db.rs`/`lib.rs`, `crates/mcp-proxy/src/hook_payload.rs` and narrow genuine hook caller, runtime `event.rs`/`in_process.rs`/`remote.rs`/`protocol.rs`, and NEW `crates/app/src/cloud_agent/completion.rs`. No UI/app.rs/global handoff/Cargo edits. Worktree `deppy-sijo-completion-proof-20261015`.
2. **PR2 original-request transport/ledger:** transport worker owns `crates/agent-mcp/src/{lib,server,history,tests}.rs` plus NEW focused `completion.rs`/tests as needed. No app/Cargo/global docs. Worktree `deppy-sijo-completion-transport-20261015`.
3. **PR3 consent/UI/integration:** settings worker owns `crates/app/src/cloud_agent/ui/{permissions,runs,management_tests,redesign_tests}.rs`, copied instructions in `ui.rs`, and `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`. Root owns `app.rs`, `cloud_agent.rs`, `access.rs`, `control.rs`, `cloud_history_worker.rs`, integration tests/docs/version/release. Worktree `deppy-sijo-completion-settings-20261015`.

Workers submit test-only commits first. Root executes real RED, then authorizes minimal implementation. Root integrates reviewed commits only; shared-file changes remain root-owned. Review against the approved design before code quality review. Do not count an unexecuted/compile-failed test as a behavioral RED.

## Agreed transport interfaces

Keep `Request.reply: SyncSender<Result<Value, String>>`; add `Request.deferred: Option<DeferredTransport>` and use None in existing in-process fixtures. The original sender is retained until durable final completion, not consumed by queued admission.

```rust
pub struct CompletionScope {
    pub workspace: String,
    pub session_id: String,
    pub generation: String,
    pub operation_id: String,
    pub run_id: Option<String>,
}
pub struct CompletionIdentity {
    pub connection_id: String,
    pub transport_session_id: String,
    pub stream_id: String,
    pub request_id: serde_json::Value,
    pub auth_epoch: u64,
    pub scope: CompletionScope,
}
pub struct CompletionReceipt {
    pub identity: CompletionIdentity,
    pub event_id: String,
    pub admission: serde_json::Value,
    pub outcome: serde_json::Value,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub expires_at: u64,
}
pub enum CompletionDeliveryState {
    Waiting, Ready, ReturnedUnacknowledged, Disconnected, Closed,
}
```

`DeferredTransport::reserve(scope, permit) -> Result<CompletionIdentity,String>` is nonblocking and reserves watch64/reader16 capacity before input. `CompletionPermit` is cloneable, revocable and nonblocking on UI; revocation fences new write admissions atomically, does not wait on socket I/O, and retires active writes under a short absolute write deadline. Every write admission checks current Auth epoch/access and permit; revocation wakes stream maintenance. Already-admitted network bytes cannot be recalled and never imply model ACK.

`Server.delivery_events` is a bounded receiver of original-identity state events, not a second final-result path. No same-credential broadcast; initialize returns a random MCP-Session-Id bound to principal, credential fingerprint and auth epoch. Ordinary missing-session JSON remains compatible. Opt-in without valid transport session fails before input. Bound request ID and scope before reserve.

`History::claim_completion_scoped` performs existing operation/context claim and pending-watch insert in ONE transaction. `finish_completion(identity, receipt)` stores one immutable bounded private final event, atomically ensuring the public admission receipt if needed. Existing `operations.outcome` remains the admission receipt: the ordinary actor/target-scoped `get_operation` must never expose a private transport-session completion. `get_completion(identity)` requires the complete original identity. Preserve operation tombstones. Pending watches are not evicted for capacity; unresolved watches become unknown on open, without restoring transport/consent/input. Terminal receipts expire after24h and max1000 stored rows.

## PR1 tasks: trusted proof and actual input sequence

- [ ] Write a baseline-compatible hook parser regression: two full prompts with the same256byte visible prefix and different suffix must retain distinct full-prompt digests while retaining identical display previews. Test through `hook_payload::read_classified`, not a made-up helper or UI title. Root executes `cargo test --offline --locked -p mcp-proxy completion_ -- --nocapture` and records the actual assertion RED.
- [ ] Compute a full bounded canonical prompt SHA digest before preview truncation. Do not retain prompt text in durable proof. Preserve genuine HookExecutionStamp independently for accepted TurnStart and matching Completed. Add bounded `storage::NativeTurnProof` to the same attention transaction/snapshot; legacy/missing runtime/turn/stamp/digest is unsupported. Explicitly prove which actual provider boundaries are eligible; do not infer support from an agent name.
- [ ] Add accepted-input regression using existing runtime fixture and event serialization, asserting a monotonic event for accepted local, tracked and encoded batches and no increment for rejected/empty input. Root runs `cargo test --offline --locked -p runtime completion_ -- --nocapture` and records behavioral RED.
- [ ] Append `RuntimeEvent::SessionInputAccepted { session, sequence:u64, operation_id:Option<String>, at_micros:i64 }` at the actual central batch-admission point exactly once. Do not include input bytes. Preserve existing enum discriminants; increase runtime wire protocol30→31 and matching assertions. This sequence is not the permission/input grant epoch.
- [ ] Add a pure watch module with typed states below and begin/admit/observe/cancel/expire operations. Store current accepted input sequence/execution/native identity and prior completion revision in each watch. Require strict empty native editor admission, its own accepted operation, a genuinely new same-execution TurnStart corroborated by full prompt digest, and matching Completed with unchanged input sequence. Manual/other input, replaced runtime/native/execution, elapsed30s start timeout, child/approval ambiguity, cancellation or restart ends unknown/expired/revoked rather than completing another task.

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalCompletionState {
    AwaitingStart, Working, Completed, Unknown, Unsupported, Expired, Revoked, Rejected,
}
```

- [ ] Test exact session/generation/actor/run/execution/native mismatch; same text as a new operation; long prompt suffix mismatch; old completion; fast start/end before observer; consumed UI badge; child/approval wait; local intervention; rejected and unknown admission; duplicate hooks; expiry. Only use complete matching stamped proof as successful completion. Run affected storage/runtime/proxy/pure module suites; root reviews no original status behavior changed. Commit PR1 unit.

## PR2 tasks: SSE transport and durable ledger

- [ ] Write HTTP tests through existing Server: two initialize calls from one credential yield distinct host session IDs; credential B using A's session is404; unknown/foreign resume cursor is404. Root executes focused agent-mcp tests and records missing-session behavioral RED.
- [ ] Implement initialized session ownership and validate optional session headers without changing off-mode JSON. Keep existing supported protocol versions and14 tools. Include opt-in/timeout/reconnect semantics in initialize/tool instructions, not a new tool or arbitrary callback.
- [ ] Add the optional deferred control handle. During initial3s, the worker accepts ordinary reply or bounded defer handoff. On handoff, release normal HTTP4 and actor2 slots, transferring original socket+receiver to a separately bounded lane. A maintenance owner retains pending receivers/cache across disconnect; completion before writer attachment is not lost. Return an explicit pre-input capacity/unsupported error if reserve fails.
- [ ] Write private loopback SSE/replay controls: priming `id: <stream>:0`, final `id: <stream>:1` with original RPC ID and standard tool result; heartbeat15s; one reader; foreign stream/session/actor/revoked credential rejects; slow client cannot block normal calls; original request disconnect/reconnect never repeats input. Root runs RED before transport implementation.
- [ ] Implement GET replay only for authenticated original session + exact Last-Event-ID capability; GET without cursor stays405. Use absolute bounded writes, watch60m/disconnected10m TTL, no unbounded buffers/threads. Revalidate and redact immediately before writing; <=1KiB final wire event. No explicit host ACK means ReturnedUnacknowledged.
- [ ] Add SQLite tests proving atomic operation+watch claim rollback, immutable exactly-one completion event, foreign full identity rejection, admission-only public operation receipts, old history migration compatibility, restart unknown and bounded terminal pruning with preserved tombstones. Run test-only RED, then implement claim/finish/get APIs. Finish record must validate payload before durable commit. No SQLite on UI/HTTP serving thread.
- [ ] Run full agent-mcp tests and independent official MCP SDK synthetic client for original POST→final SSE→disconnect→resume. Record actual package/SDK version and raw result. Root reviews cancellation fences, resource limits and compatibility, then commit PR2 unit.

## PR3 tasks: root integration and readable consent

- [ ] Root adds `Permission.local_completion` default false, `set_connection_local_completion(id,target,enabled)` and exact read-consent gates. Existing read revoke/opt-out/connection revoke/expiry/rotate/stop/session replacement retire watches and transport permits. Input-only revoke does not erase already confirmed read-authorized receipts. No consent restoration after restart.
- [ ] Root routes bounded NativeTurnProof snapshots from existing agent-state worker to cloud Target without DB access in a frame. Route new accepted-input events for active/warm runtimes and exact Target execution identity. Do not use global_turn_done/quiet output/progress100. Add a completion tracker field and separate `CompletionStatusView { local: LocalCompletionState, delivery: CompletionDeliveryState }` getters for target/operation.
- [ ] Root reserves completion before durable claim; adds explicit completion claim/finish jobs to bounded HistoryWorker and result handling. Dispatch only after successful atomic claim. Retain original reply through accepted admission and watch observation; persist immutable final receipt before replying. Capacity/failed claim/disconnected/revoked outcomes must not silently type or duplicate input. Current3s admission ceiling remains; no automatic input retry.
- [ ] UI worker writes real egui interaction tests using existing connection_fixture/harness: default-off, disabled no-read, enable with read/no input/no lease, actor/session isolation, read revoke resets, input revoke preserves, ended/revoked disables, local Completed plus delivery Disconnected shown separately, returned never ACK, geometry at720px. Root runs baseline RED before UI changes.
- [ ] Add one existing-table completion checkbox column with34px hit region; reuse existing hint/fact/status components. Use exact shared API getters; separate local/delivery facts in run details. Explain host long-request/reconnect support remains unverified. Add all five locale keys in identical order and update copied MCP instructions. No popup or new permission grant.
- [ ] Integration RED/GREEN through actual guarded admission: opt-in off legacy JSON; on original RPC waits until verified own turn; input sequence intervention aborts; identical prompts in independent sessions remain independent; quick completion, revoked pending claim, worker DB failure, denied/unknown input, old generation and late secret registration. No real provider messages/user DB. Cross review transport→proof, proof→UI, UI→transport and root final review.

## Final gates and delivery

- [ ] `cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1` (all actual harness summaries; exclude nested subprocess duplicates).
- [ ] `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cargo run --offline --locked -p xtask -- check-boundary`, `cargo run --offline --locked -p xtask -- check-deps`.
- [ ] Update handoff/report/evidence with exact source, RED/GREEN, failure corrections and remaining real-provider compatibility limits. Commit and push PR units; verify exact-head CI before merging necessary changes to main. Preserve original dirty worktrees.
- [ ] For user-delivered build increase0.12.3→0.13.0 in workspace Cargo and inherited lock packages, changelog; release/package from reviewed main. Verify CLI + CFBundleShortVersionString + CFBundleVersion all0.13.0, signature/archive/architecture. Preserve0.12.3 bundle. Do not launch or restart the app.

## Self review

All approved design sections map to proof/input(PR1), original identity/ledger/limits/replay(PR2), opt-in/revocation/worker/UI(PR3), or final validation. Exact proof provider eligibility must be demonstrated in code/tests, otherwise reject before input and show unsupported. Optional provider callbacks and MCP Tasks are not part of this approved implementation. No new dependencies or alternate providers are assumed.
