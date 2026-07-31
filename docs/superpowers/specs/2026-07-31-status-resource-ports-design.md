# Status Bar Resource and Port Management Design

## Objective

Deliver the requested sidebar and status-bar refinements while adding Orca-inspired resource and port inspection without duplicating Deppy's existing process sampling or introducing idle polling bursts.

The feature must:

- remove workspace session-count text while preserving the state dot,
- increase workspace avatars from 15×15px to 18×18px,
- remove the `터미널` status-bar label,
- make app/session resource usage open a detailed manager,
- add a status-bar port summary that opens a detailed port list,
- refresh data on demand,
- identify and terminate only provably unattached local sessions,
- terminate a listening process only after backend ownership revalidation,
- represent unsupported SSH metrics as unavailable rather than zero.

## Chosen Approach

Use existing runtime resource snapshots and add bounded, on-demand control surfaces.

This differs from copying Orca wholesale:

- Deppy already samples local runtime process resources every two seconds, so the resource manager reuses those snapshots instead of adding another OS process poller.
- Port scans run when the popover opens or the user requests refresh. Continuous visible-only polling can be added later if measurements justify it.
- Resource refresh invalidates the app projection cache; it does not restart a terminal daemon.
- Destructive actions are revalidated in the owning backend immediately before execution.

## UI Design

### Workspace rows

- Keep the existing status dot and status-color priority.
- Remove the numeric total-session label and its reserved width.
- Increase the avatar rectangle to 18×18px while retaining vertical centering and the existing initial glyph size.
- Preserve the session rows and `SidebarSessionSummary`; this is a presentation-only change.

### Status bar

- Remove the `터미널` view label and its leading separator.
- Keep the existing CPU, app memory, and session memory values, but render them as one focusable/clickable resource segment.
- Add a separate focusable/clickable `포트 N` segment using the latest cached successful scan. Before the first scan, render `포트 —` rather than zero.
- Keyboard activation and accessible labels must match pointer activation.

### Resource manager

Open an anchored popover from the resource segment. It contains:

- header: title, safe data refresh, close,
- summary: app CPU, app RSS, session RSS, session count, last sample age,
- hierarchy: workspace rows with expandable session rows,
- session details: state, CPU, RSS, process count, pressure indicators, local/remote availability,
- actions: focus session, terminate one session, review unattached sessions, terminate confirmed unattached sessions.

The popover uses a bounded scroll area and virtualized/visible-row rendering where supported by the existing egui patterns. It stores identifiers and small presentation state only; it does not retain terminal snapshots, scrollback, or process output.

### Port manager

Open an anchored popover from the port segment. It contains:

- header: title, manual refresh, close,
- summary: workspace count, owned listener count, external listener count,
- groups: active workspace, other workspaces, external/unowned,
- rows: port, process name, PID, bind address, protocol, workspace, ownership source,
- safe actions: copy address, open HTTP(S) address when applicable, terminate an owned local listener.

External, container-owned, Deppy-owned, ambiguous, and remote entries are read-only.

## Resource Data Flow

1. Each local runtime continues producing its existing two-second bounded resource event.
2. App composition projects those events into the existing bounded `ActivitySnapshot` and 500ms presentation cache.
3. The status bar renders only the latest immutable projection.
4. Opening the resource manager performs no OS command and starts no timer.
5. Resource refresh invalidates the presentation cache and requests an immediate repaint. It does not fan out simultaneous OS samples to every runtime.
6. SSH sessions lacking the resource event capability render `—` and an unavailable label.

## Unattached Session Safety

The UI never decides that a process is safe to kill.

Add a runtime-owned command that atomically recomputes candidates immediately before termination. A session is terminable as unattached only when all conditions hold:

- it is owned by the target local runtime,
- it is not referenced by any mux tab or pane,
- it has no active remote-viewing/restore lease,
- it has no pending attachment or restore transition,
- it still exists at execution time.

Unknown ownership fails closed. Remote sessions are never included in the local unattached-session action.

The manager first shows the current candidate count. Destructive execution requires confirmation and uses the runtime's recomputed set, not the stale UI list.

## Port Data Flow

### Scan worker

Use one app-owned worker outside the render path.

- macOS MVP command: `lsof -nP -iTCP -sTCP:LISTEN -F pcn` plus bounded point lookups for cwd/command when needed.
- one in-flight request; a newer refresh request replaces or follows the current request without spawning parallel scans,
- command timeout: 4 seconds,
- captured output limit: 2 MiB,
- retained result limit: 200 listeners,
- diagnostics are low-cardinality and do not expose arbitrary command lines or paths,
- worker shutdown cancels the command, reaps children, and joins the worker.

Workspace ownership uses, in order:

1. process cwd contained by a known local workspace,
2. bounded command-path evidence,
3. longest matching workspace root.

Ambiguous or missing evidence produces an external/unowned row.

### Listener termination

The UI sends only the stable scan identity: workspace ID, PID, port, protocol, and bind address.

Before signaling, the backend performs a fresh bounded scan and confirms:

- the PID still owns the same listener,
- the listener still maps to the same workspace,
- the process is not Deppy, a protected agent owner, external, container-owned, or ambiguous.

Only then send `SIGTERM`. Never use `SIGKILL` from the UI action. Refresh immediately after completion and once more after a bounded delay using the existing event/repaint path rather than a permanent timer.

## Resource and Burst Limits

- No new idle resource polling for the resource manager.
- No port scan before the first explicit open/refresh.
- At most one port scan command tree at a time.
- At most 200 retained port rows and 2 MiB captured output.
- Existing attachment/session/runtime limits remain authoritative.
- Bulk unattached termination snapshots only stable identifiers and processes targets in a bounded sequence.
- No terminal data, scrollback, environment variables, or complete command lines are retained by either popover.

## Error Handling

- Resource snapshot unavailable: keep the last good snapshot and display stale age.
- Remote metrics unsupported: display `—`, never `0`.
- Port command unavailable or timed out: keep the last good result, display a refresh error, and apply bounded backoff to repeated automatic retries. Manual retry remains available after the current request settles.
- Ownership changed before termination: reject without signaling and refresh the list.
- Runtime candidate set changed: terminate only the recomputed safe set and report the resulting count.

## File Ownership and Parallel Implementation

The implementation uses disjoint write lanes:

1. Workspace-row lane: `crates/app/src/ui/file_tree.rs` only.
2. Resource UI lane: new `crates/app/src/ui/resource_manager.rs`, status presentation extraction, and owned locale keys.
3. Runtime safety lane: `crates/runtime/src/command.rs`, `crates/runtime/src/event.rs`, `crates/runtime/src/in_process.rs` only.
4. Port worker/UI lane: new bounded scanner and port UI modules, without modifying runtime files.
5. Orchestrator integration lane: `crates/app/src/app.rs`, `crates/app/src/ui/mod.rs`, final locale merge, handoff, tests, and review.

No two active workers may edit the same file. Integration files remain orchestrator-owned until all lower lanes are reviewed.

## Test Strategy

All behavior follows RED-GREEN TDD.

Focused regressions:

- workspace row shows an 18px avatar, retains its status dot, and emits no numeric session count,
- status bar emits no `터미널` label and exposes accessible resource/port buttons,
- closed popovers perform no host work,
- resource popover renders app/workspace/session rows from immutable snapshots,
- unsupported remote resources render unavailable,
- unattached classification protects pane, lease, restore, pending attachment, and remote cases,
- runtime revalidates before bulk termination,
- port parser handles IPv4/IPv6/wildcard listeners and rejects malformed/oversized input,
- scanner enforces timeout, byte/item ceilings, one in-flight operation, and child reaping,
- nested workspace ownership selects the longest valid root,
- port termination sends no signal when fresh ownership differs,
- protected/external/ambiguous listeners expose no terminate action.

Final gates:

- focused module tests,
- full app and runtime tests,
- strict Clippy with warnings denied,
- rustfmt check,
- diff check,
- signed macOS package verification,
- physical inspection of both popovers and destructive confirmation flows.

## Deferred Scope

- SSH CPU/RAM and remote listener collection,
- continuous 30-second port polling,
- port forwarding creation/editing,
- terminal-output URL detection,
- a separate terminal daemon restart control,
- long-term resource sparkline persistence across app restarts.
