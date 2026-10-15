# Cloud Agent Management Implementation Plan

> **For agentic workers:** Execute the six approved PR units with delegated, disjoint file ownership. The root agent reviews every integration and owns commits. All delegated models are `gpt-6.1-sol`, reasoning effort `xhigh`.

**Goal:** Manage authenticated cloud connections, per-session access/control, durable cloud work and receipts, bounded output history, reconnect profiles, and the same results on desktop/PWA.

**Architecture:** Keep the native App as the sole terminal-effect authority and the existing history worker as the sole SQLite owner. Authentication publishes a non-secret principal; a durable run/operation model records responsibility and status; the App validates identity, generation, access, lease, auth and deadlines before effects. PWA consumes an operator-authenticated, bounded snapshot and sends explicit intents to the App.

**Tech Stack:** Rust, egui, SQLite/rusqlite, MCP POST JSON, existing runtime/PTYS and web-remote JavaScript.

## Approved scope and execution rules

- Source baseline: `3ac1f61f` / 0.12.1, `/Users/jr/Desktop/projects/deppy-sijo-performance`. The October 13 review and user approval authorize implementation of PR 1–6.
- Do not restart the running app, modify real user terminals, or perform real provider login/task mutations. No release artifact is delivered by this coding task; release version validation belongs to a later explicit release/build delivery request.
- Preserve existing `docs/mockups/active-work-visibility-2026-10-12.html`. Root maintains `docs/CODEX_HANDOFF.md` and commits reviewed units.
- First meaningful behavioral tests precede implementation. Each owner records the exact red/green commands and results. Root serializes workspace Cargo invocations to avoid shared build races; pure JavaScript checks may run independently.
- Compatibility: retain existing six MCP tools and legacy operation tombstones; new parameters/tools are additive. Do not infer completion from queue admission or a quiet/stale screen. Revoke pending input at actual admission. No uncertain input replay after reconnect.
- Provider-specific E2E evidence requires an actual supported provider account. Local HTTP/OAuth/PTY fixtures are automated evidence, not a claim of dot/Muse/Grok Bot account certification.

## File ownership

| Owner | Files | Responsibility |
| --- | --- | --- |
| cloud_auth | `crates/agent-mcp/src/server.rs`, `oauth.rs`, `lib.rs`, `tests.rs` | Trusted principal, connection registry/last activity/revocation, MCP tool schemas and safe errors |
| cloud_domain | new `crates/agent-mcp/src/management.rs`, existing `history.rs`, `crates/app/src/cloud_history_worker.rs` | Run lifecycle, scoped durable receipts, migration, bounded queries, async worker |
| cloud_settings | `crates/app/src/cloud_agent/ui.rs`, new settings helper modules, `crates/i18n/locales/*/messages.txt`, `docs/cloud-agent-mcp*.md` | Server/client state distinction, connection/access/control/run UI, onboarding and instructions |
| root | `crates/app/src/cloud_agent.rs`, new App bridge helpers, `config.rs`, `app.rs`, `ui/cloud_answer.rs`, plan/handoff | Access/leases, run/effect integration, output/freshness, profiles/reconnect, publication and review |
| later PWA owner | `crates/web-remote/src/*` and its owned frontend files | Bounded operator snapshot, cloud results and controls, mobile layout and UI tests |

No owner edits another owner's file without root's explicit handoff. `lib.rs` module/export wiring is cloud_auth-owned. New management definitions are cloud_domain-owned and exports are requested by message. Root may make narrow integration repairs only after the owner has stopped editing the file.

## Shared contracts to freeze before integration

- `Principal`: connection ID, display name, auth kind, input scope, authentication/expiry timestamps. IDs come from verified credentials, never `clientInfo` or tool arguments. OAuth refresh preserves connection identity. Legacy manual bearer has an explicit single connection identity; independently minted credentials may identify additional manual connections.
- `ConnectionInfo`: principal plus last authentication/initialize/tool-success timestamps and active/revoked/expired state. Registry/revoke operations are local-owner APIs; an agent cannot manufacture another actor or request broader rights.
- `OperationContext`: connection ID, external operation ID, run ID and session generation. Scope new internal operation keys to the authenticated connection without changing old at-most-once records.
- `CloudRun`: ID, connection ID/name, workspace/session/generation, title/instruction, reported state/progress/message, created/updated/heartbeat timestamps, result/artifacts, cancel request/ack. Use bounded strings/counts and legal state transitions; restore unfinished runs as unknown/stale.
- Additive tools: `start_run`, `update_run`, `get_run`, `list_runs`, `read_history`, `get_operation`, `acquire_control`, `release_control`. Existing input/notify tools accept optional `run_id`. Treat external cancellation as a request until the same actor acknowledges it.
- Settings/UI owns intents only. Root owns side effects. Per-connection access is constrained by original session sharing/generation; input has one connection/run lease per session, user takeover revokes queued admission. A read-only client cannot acquire write control.
- Output history is cursor/range and byte limited, redacted and generation-scoped, with observation time and truncated/reset markers. Reuse terminal history; do not store unlimited logs in the App.
- Connection profiles persist only non-secret coordinates/display preferences. Authorization/input is not silently restored. Reconnect is bounded, opt-in where needed, and never replays terminal input.
- PWA payload contains authorized operator data only, excludes auth tokens/fingerprints/credentials, and is bounded by the same retained model. Cloud controls traverse the App rather than writing directly from the web thread.

## PR 1 — setup state and guidance

- [x] Test that starting a local listener never labels a provider authenticated; automatic public readiness and client activity are separate.
- [x] Use native shared UI components and add `paste_text`/byte limits/receipt rules to the common copied instruction.
- [x] Describe automatic URL default and stable manual endpoints; record provider compatibility matrix with actual evidence status.
- [x] Root review, targeted UI test + i18n checks, commit.

## PR 2 — connection identity, permissions and control

- [x] RED: refresh keeps one principal; revoke A leaves B active; unauthenticated/client-supplied names cannot become authority.
- [x] RED: same external operation ID belongs independently to two actors; A cannot read B receipt or target; session generation change invalidates permission; local takeover invalidates waiting input.
- [x] Implement registry, per-connection scopes/grants, explicit input lease and expiry; expose owner actions and settings list.
- [x] GREEN: OAuth/HTTP and actual PTY admission fixtures for independent/different sessions and same-session control conflict.
- [x] Root review and commit with regression evidence.

## PR 3 — cloud runs and authoritative status

- [x] RED: queue receipt does not complete a run; terminal transitions are legal; actor/session/generation ownership fences apply; heartbeat expiry/restored unfinished run becomes stale/unknown.
- [x] Persist bounded cloud runs and artifact references through the worker; implement additive run tools and original-session notify linkage.
- [x] Display cloud reported phase separately from local terminal state. Cancellation request/ack, input revocation and Ctrl+C remain distinct.
- [x] GREEN: two concurrent runs, reused IDs, report loss, delayed notify, restart, wrong actor and original-pane result fixtures.
- [x] Root review and commit.

## PR 4 — history, freshness and receipts

- [x] RED: viewport alone cannot supply older errors; bounded history cursor/reset/truncation is explicit; scoped receipt lookup has zero effects and no cross-client disclosure.
- [x] Add generation-scoped terminal history effect/reply and freshness metadata, operation lookup, normalized error fields and bounded poll budget.
- [x] GREEN: output beyond screen, redaction, Unicode/bounds, lost replies, unknown receipt and polling contention while UI/other actor remains responsive.
- [x] Root review and commit.

## PR 5 — connection continuity

- [x] RED: profile roundtrip never persists a bearer/approval; bounded retry resets at success, stops on explicit disconnect/auth expiry, and never repeats input.
- [x] Add named non-secret profiles, stable-host mode, expiry/re-auth UI, wake/health revalidation and capped backoff.
- [x] GREEN: simulated tunnel failure, sleep-sized heartbeat gap, rotation/expiry, explicit stop, changed URL and changed generation.
- [x] Root review and commit.

## PR 6 — results and PWA

- [x] RED: desktop/mobile snapshots retain the original session/actor/run and contain no credentials; web controls require existing operator auth and go through App validation.
- [x] Add searchable/filterable bounded run/result history and artifact links, desktop source labels, mobile cloud results/control view and pane navigation.
- [x] GREEN: operator-authenticated WebSocket fixtures and narrow/mobile browser tests, stale/unknown/cancel displays, API compatibility harness for old and new tools.
- [x] Root review, final suite/clippy/fmt/boundary checks, commit; provider account certification status remains explicit.

## Commands and review gates

Run from the current worktree, with Cargo invocations scheduled by root:

```sh
cargo test --locked -p agent-mcp
cargo test --locked -p deppy-sijo cloud_agent::
cargo test --locked -p deppy-sijo cloud_history_worker::
cargo test --locked -p web-remote
cargo clippy --locked -p agent-mcp -p deppy-sijo -p web-remote --all-targets -- -D warnings
cargo fmt --all --check
cargo run --locked -p xtask -- check-boundary
git diff --check
codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh --help
```

Actual source diffs are reviewed by root and a `codex exec/review` call with the required model/effort before commits; use a read-only review prompt listing only changed source files. Do not treat a CLI capability/help check as a code review or a test PASS.

## Initial orchestration state

- [x] Current source, release handoff, git state and approved review checked.
- [x] Contracts, ownership, dependencies and six delivery units recorded.
- [x] Dispatch three parallel owners; root integrates only after contracts are published.
- [x] Track per-PR code review, executed checks and commit SHAs in handoff.

## Final delivery — 2026-10-13

- PR1 source `35e8a264`; PR6 web layer `1878a449`; output-secret regression fixes `2fdf4a4d`; internal runtime ID compatibility `1dd6d16d`; reviewed native/auth/domain/PWA integration `9d0ed76b`. Initial plan `19fa8df3`.
- PR2–6 cross-crate principal/request/run/worker/operator contracts land atomically in the integration commit so the retained history is compilable; secret/runtime and the earlier independent web layer remain separate coherent units.
- Final executed evidence and account-testing limits: [acceptance report](../../qa/2026-10-13-cloud-agent-management.md). All approved implementation work and root reviews are complete. No push, product release artifact or app restart was performed.
