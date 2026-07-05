# Rust AI Agent Workspace Final Architecture v2.7 Optimization

작성일: 2026-07-05

기준:
- v2.6 final architecture with folder tree
- v2.8 persistence/store improvement final addendum
- Review Track findings in `docs/review/`
- Build Track summaries in `docs/build/`
- 기준 커밋: `b7bab02 Implement third build track wave`

## 1. Purpose

This document records the implemented and optimized state after the Review Track
and the first three Build Track waves. It does not replace v2.6 or v2.8. It is a
living implementation baseline for the update-only PR-U hardening track.

## 2. Core Invariants

1. UI sees runtime through `RuntimeClient`.
2. Runtime coordinates mux, session, env, terminal, and pty.
3. Only visible panes in the active workspace/window active tab may render.
4. Hidden workspace/session work is limited to log, status, and lightweight state.
5. `TerminalViewportSnapshot` is created only for visible panes.
6. Raw plaintext logs are disabled by default.
7. Session does not directly know the secret store.
8. Folder tree, drag-and-drop path insert, terminal copy, and paste UX are non-regression surfaces.
9. `mcp`/`audit`/`persist`/`storage` layers must remain acyclic.
10. Secret/env/API key values are not stored in DB/config/log/export as plain text.

## 3. Implemented Baseline

The current implementation includes:

- Pane-level runtime behavior.
- Workspace, pane, mux, and session relationships.
- Folder tree display for valid workspace roots.
- Folder tree and sidebar path insertion into terminal.
- Terminal selection, copy, paste, bracketed paste, and CJK wide-character selection normalization.
- RuntimeClient-based UI/runtime split for terminal operations.
- Redacted log and MCP audit default-off encrypted blob behavior.
- Secret-like env and args persistence guards.
- Storage graph hygiene with `mcp -> rusqlite` removed and `xtask check-deps` passing.

## 4. Completed Optimization Work

### Boundary Hardening

PR-B00 removed direct leaf-UI secret-store usage for credentials and connectors,
introduced app-level service adapters, and added `cargo run -p xtask -- check-boundary`.
Remaining DB/MCP/audit UI exceptions are explicit and frozen by file/snippet/count.

### Dependency Hygiene

PR-B01b removed the direct `mcp -> rusqlite` dependency. The accepted v2.8 reality
is a legal DAG where `storage` remains a composition/app-level facade, while
`storage-core` and `mcp-store` avoid runtime/domain cycles.

### Pane Visibility Guard

PR-B02a prevents stale hidden viewport events from rehydrating UI snapshot cache
after mux visibility changes. Remote server/client baselines are pruned to visible
sessions after `MuxUpdated`.

### Folder Tree and DnD

PR-B03a added shell-specific path quoting and empty-root handling. PR-B03b moved
directory listing off the UI thread, applies listing chunks with bounded per-frame
drain, and discards stale listing results by root epoch and directory token.
PR-U16 adds watcher-side generated-path ignore rules, debounce batching, dirty
directory deduplication, `.env*` path-level warning candidates, and a per-window
reload cap.

### Terminal Paste and CJK

PR-B04a normalized wide-character selection endpoints. PR-B04b unified clipboard
paste, path insert, and drag/drop paste byte generation through the terminal input
mapper with bracketed-paste support and no auto-Enter.

### Redaction and Audit

PR-B05a and its review fix block secret-like env/arg persistence, including
one-line `--api-key value`, `--database-url=...`, and secret-like `KEY=VALUE`
arguments. PR-B06a keeps encrypted audit blobs explicit opt-in/default-off and
redacts unsolicited MCP debug logging.

### Idle Repaint

PR-B08a removed the idle approval repaint loop and normalized `output_batch_ms`
to the documented lower bound.

### Status Detector Cost

PR-U17 keeps the existing stream/screen/idle status detector semantics but adds
a pre-scan gate so regex-empty sessions do not build backend screen text on every
output tick. It also exposes lightweight detector cost stats.

### Process Resource Monitor

PR-U12 adds a low-cadence runtime process sampler that emits app process CPU/RSS
snapshots through `RuntimeEvent::ResourceUsage`. This is the first resource
monitoring layer; per-session child process tree aggregation and UI controls are
still pending.

### Terminal Cache Budget

PR-U14 adds explicit terminal cache classes and budgets. Visible sessions retain
up to the visible budget, hidden/exited sessions are capped by tighter line/byte
budgets, and runtime archive decisions consider global estimated terminal cache
pressure while preserving active visible sessions.

### SQLite Write Batching

PR-U18 adds a bounded background `DbWriteWorker` foundation for coalescing session
status updates, log offset updates, and pending approval inserts. Runtime/app
hot-path wiring remains a follow-up before the performance gate can claim full
SQLite write batching coverage.

### Output Backpressure

PR-U15 adds a bounded in-process runtime command queue. Overflow is surfaced to
the caller instead of blocking the UI thread or growing memory without bound.
PTY output remains bounded by the reader channel and viewport delivery remains
latest-wins/coalesced.

## 5. Current Runtime Model

The intended runtime model remains:

```text
App/UI
  -> RuntimeClient
    -> Runtime
      -> mux/session/env/terminal/pty
```

`crates/app/src/app.rs` is treated as a composition root exception for wiring
concrete stores and services. Leaf UI modules should receive service traits or
runtime clients rather than constructing secret, terminal, pty, or session
backend implementations directly.

## 6. Current Persistence Model

The current persistence graph follows the v2.8 accepted addendum:

```text
storage-core -> rusqlite/anyhow only
mcp-store    -> rusqlite/serde_json
persist      -> core, mux
storage      -> storage-core, mcp-store, audit, persist, core, secret
app/proxy    -> storage facade where needed
```

This is a legal DAG. Further crate splitting is not required unless new cycles,
new repository ownership conflicts, or release gates show a concrete problem.

## 7. Remaining Optimization Tracks

### Phase D

Pending work:

- Session child process tree aggregation and resource-control UI.
- Workspace auto suspend.
- Terminal cache budget manager.
- Output pipeline backpressure.
- File watcher debounce and ignore rules.
- Status detector cost control.
- SQLite write batching.
- Remote slow consumer backpressure.
- Final performance gate.
- Terminal dirty-range partial render, currently documented as PR-U26.

Current status: file watcher debounce/ignore and status detector cost gating are
implemented. Terminal cache budgeting and remote slow-consumer backpressure are
implemented. SQLite write batching foundation exists, but call-site migration is
pending. Process monitoring has app-process sampling, but session child process
tree aggregation is still pending. Output pipeline backpressure now covers the
local runtime command queue; PTY input queue policy and user-visible pressure UI
remain pending.

### Phase E

Pending work:

- I18n crate and required catalogs.
- UI string migration.
- Runtime/notification message localization.
- I18n/CJK layout gate.
- Global activity view.

## 8. Release Gates

Release requires:

- PR-U11 Final Security Gate.
- PR-U20 Final Performance Gate.
- PR-U24 I18n / CJK Layout Gate.

Final merge criteria:

- Critical/High finding count is zero.
- Hidden pane snapshot creation is zero.
- Raw plaintext log default storage is zero.
- Crate cycles are zero.
- CJK path drag/drop works.
- Terminal copy/paste works.
- RSS/CPU goals are met.
- Required locale key completeness is 100%.

## 9. Operational Rule

Subsequent PR-U work must update `docs/update/update-findings-summary.md` and add
a `docs/build/PR-Uxx-summary.md` before merge. Already completed PR-B work should
be skipped unless a new regression is reproduced.
