# Multi Cross-Workspace Pane Design

## Objective

Keep workspace A active while showing up to six terminal panes owned by other
workspaces in the same central surface. A remains the leftmost primary pane.
Foreign panes append on the right, can be reordered by dragging their headers,
and never transfer tab, pane, session, or runtime ownership into A.

The feature must stay event-driven at idle, avoid simultaneous cold-start
bursts, release runtime protection after detach, and render only visible panes.

## Approved Entry Points

- Drag any eligible session row from another workspace into the central pane
  strip.
- Reveal an `Open beside` icon when hovering an eligible session row.
- Keep the existing right-click `Open in current view right` action.
- Do not add a command palette entry.
- Do not add a pane-header workspace picker.

All three retained entry points emit one `OpenBeside(SessionRowTarget)` action.
The drag payload contains identifiers only; it never contains scrollback,
terminal snapshots, titles, cwd strings, or runtime handles.

## Session Row Targets

Session rows use an explicit target enum instead of fake live identifiers.

```rust
pub(crate) enum SessionRowTarget {
    Live {
        workspace_id: String,
        runtime_instance: u64,
        tab: MuxTabId,
        pane: MuxPaneId,
        session: SessionId,
    },
    PersistedPane {
        workspace_id: String,
        pane: MuxPaneId,
    },
}
```

- `Live` attaches immediately after exact mux relation validation.
- `PersistedPane` requests bounded background materialization, then attaches
  only after the runtime publishes an exact pane/session relation.
- An agent pane is restored as its existing read-only archive and is never
  automatically restarted.
- A persisted shell pane starts one fresh shell at its saved cwd and replays
  its bounded saved ANSI tail, matching the current restore contract.
- A historical shell session no longer linked to `mux_panes` is not a pane and
  is therefore not advertised as draggable. Creating a new shell from session
  history remains a separate action rather than pretending the old pane still
  exists.

## Attachment State

The state is ordered and bounded.

```rust
pub(crate) struct AttachmentId(u64);

pub(crate) struct CrossWorkspacePaneState {
    primary_workspace_id: Option<String>,
    attachments: Vec<AttachedPane>,
    focused: FocusedSurface,
    next_id: u64,
}

pub(crate) enum FocusedSurface {
    Primary,
    Attached(AttachmentId),
}

pub(crate) enum AttachedPaneState {
    Restoring(PersistedPaneRequest),
    Live(WorkspacePaneTarget),
    Placeholder(AttachedPlaceholder),
}
```

The vector has a compile-time hard cap of six. The persisted user setting is
the admission cap, defaults to two, and is normalized to `1..=6`. Primary is
not counted.

- A new foreign pane is always pushed to the right.
- An exact duplicate focuses the existing attachment.
- Reorder changes only the vector order and emits no runtime command.
- Foreign panes reorder only among foreign panes; A stays fixed left.
- Detach removes only the view reference. It sends no close, kill, split, or
  pane-move command to the source runtime.
- Lowering the setting detaches rightmost excess views immediately. This is
  non-destructive and promptly releases their runtime protection.
- Attachment order, width, and live targets are not persisted across app
  restarts. Only the maximum count setting is persisted.

Each attachment owns a finite pixel width. The default is 420 px, the minimum
is 320 px, and the maximum is 960 px. Non-finite values use or retain the safe
default. Width is not modeled as recursively nested split ratios because that
would make placement depend on insertion history.

## Layout and Reorder

The central surface is one horizontal strip:

```text
[ A primary ][ B foreign ][ C foreign ][ D foreign ]
```

- A gets the remaining width but never less than 320 px.
- Foreign panes keep their bounded pixel widths.
- If the total width exceeds the viewport, the strip scrolls horizontally.
- Header drag shows insertion markers only between foreign panes.
- Dropping a sidebar session row on the central surface appends it at the far
  right. The user can then drag the new foreign header to the desired order.
- Vertical or adaptive split placement is out of scope.

The render pass computes all pane rectangles first. A pane whose rectangle
does not intersect the horizontal viewport is not painted and does not drain
native input. It retains only its small attachment record and source runtime
lease.

## Runtime Preparation and Rendering

`WorkspaceUi` currently combines runtime-event preparation with one pane
render. Multi-pane support separates those responsibilities:

1. Group visible attachments by `(workspace_id, runtime_instance)`.
2. Drain and apply each owning runtime's pending events once per frame.
3. Refresh each runtime's terminal cache once.
4. Render every visible pane from that prepared cache.
5. Route keyboard, IME, clipboard suppression, composer input, and host I/O to
   exactly one focused surface.

For multiple panes from one runtime, closing one view must not change that
runtime to Warm while another visible view remains. Runtime visibility is
derived from a set of visible runtime identities, not a single target.

Attached runtimes are protected from eviction while referenced, but they still
count toward the resident/live runtime budget. Protection prevents destructive
eviction; it does not create a capacity exemption. If the configured live
budget cannot admit another runtime, the request remains bounded and reports a
localized capacity message instead of oversubscribing memory.

## Cold Materialization

The existing `RestoreWorkspace` restores every pane and can spawn several PTYs
for one dropped row. Cross-workspace cold restore must not call it.

Add a bounded runtime command that materializes one persisted pane:

```rust
RuntimeCommand::RestoreWorkspacePane { pane: MuxPaneId }
```

On the first request for a cold workspace, the runtime loads the existing
bounded restore snapshot once, reconstructs the tab/pane skeleton without
starting unrequested sessions, and materializes only the requested pane. Later
requests reuse the same runtime and skeleton. When the user actually switches
to that workspace, the normal activation path materializes the remaining panes
sequentially before considering the restore complete.

The App owns a restore coordinator with these invariants:

- At most six queued requests.
- At most one cold workspace/pane materialization in flight globally.
- Requests are deduplicated by `(workspace_id, pane_id)`.
- A restoring placeholder is appended immediately so ordering is stable.
- Multiple requests for one workspace reuse one runtime instance.
- A completion is accepted only when workspace, runtime incarnation, pane, and
  runtime session all match the queued request.
- Detaching a queued placeholder cancels admission. Detaching an in-flight
  placeholder marks its result ignorable; it does not kill a worker or PTY.
- No polling thread or periodic timer exists while the queue is empty.
- One finite deadline exists only for the current in-flight request.
- Agent auto-resume and agent command launch paths are never invoked.

The runtime keeps only the bounded restore catalog needed for unmaterialized
panes. Entries are removed as panes materialize. The catalog is dropped after
the remaining workspace is materialized or when the runtime is evicted.

## Memory and CPU Contracts

- Hard attachment cap: 6.
- Default user cap: 2.
- Global cold materialization concurrency: 1.
- No terminal snapshot or scrollback clone in drag payload or attachment state.
- No new idle worker thread, periodic repaint, or polling loop.
- Offscreen pane paint is skipped.
- Runtime events are drained once per runtime per frame.
- Native input is drained once by the exact focused live surface, or discarded
  once when no surface is eligible.
- Detaching the last pane of a foreign runtime removes its protection and
  refreshes the normal idle-eviction deadline.
- Queue, placeholder, timeout, and pending completion records are removed on
  success, failure, cancellation, primary workspace switch, and shutdown.
- The feature does not persist attachment state and therefore adds no startup
  replay allocation.

## Failure UX

- Capacity reached: keep existing panes and show a localized message.
- Runtime budget reached: do not create a runtime; keep no hidden process.
- Persisted pane disappeared: remove the placeholder and show a localized
  unavailable message.
- Restore deadline expired: remove the placeholder, release protection, and
  show a retryable message.
- Runtime incarnation changed: reject stale completion and reconcile against
  the new runtime without falling back to A.
- Spawn failure: leave source persistence untouched and remove the placeholder.

## Tests and Evidence

The implementation uses strict RED/GREEN cycles for each lane.

Required deterministic tests:

- Default 2 and normalization to `1..=6`.
- Append-right ordering, duplicate focus, stable ID focus after reorder,
  rightmost capacity trim, and non-finite width handling.
- Live and persisted row payloads are identifier-only.
- Hover button and right-click emit the same target action.
- Drag threshold suppresses ordinary row click/workspace switching.
- Persisted projection retains exact workspace and pane IDs with existing row
  and aggregate byte admission.
- `RestoreWorkspacePane` materializes only one requested pane.
- Agent pane materialization does not emit an agent-start command.
- Restore coordinator admits one in-flight request and deduplicates requests.
- Cancellation and timeout release queue state and runtime protection.
- Multiple visible panes from one runtime drain events once.
- Offscreen panes are excluded from the render set.
- Header reorder emits no runtime command.
- Closing one of two panes from one runtime retains visibility; closing the last
  releases it.
- Focus, IME, clipboard, composer, and host I/O route to one exact attachment.

Final gates:

```bash
cargo test -p storage --locked list_persisted_activity_panes_bounded -- --nocapture
cargo test -p runtime --locked restore_workspace_pane -- --nocapture
cargo test -p deppy-sijo --locked cross_workspace -- --test-threads=1
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo test -p deppy-sijo --locked -- --test-threads=1
git diff --check
```

Resource evidence is measured on the same machine against the current
`DesignALL` baseline. The new idle layout must remain event-driven, cold
materialization concurrency must peak at one, and detaching all foreign panes
must let the existing eviction path reclaim their runtimes. RSS is recorded as
measurement evidence rather than a brittle absolute unit-test threshold.

## Non-Goals

- Moving source mux panes into A.
- Vertical/adaptive layout.
- Unlimited pane count.
- Persisting cross-workspace attachments.
- Restarting stopped agents automatically.
- Reconstructing a historical shell process that no longer has a canonical
  pane.
- Command palette or pane-header workspace picker.
