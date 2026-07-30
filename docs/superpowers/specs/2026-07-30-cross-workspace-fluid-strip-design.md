# Cross-Workspace Fluid Pane Strip Design

## Objective

Remove unused space to the right of attached workspace panes, make the boundary between the primary workspace and the first attached pane directly resizable, preserve per-pane resizing for additional attached panes, and replace the attached-pane title with `Project (Workspace)`.

## Approved Layout Model

Use a one-dimensional horizontal fluid strip rather than a split tree.

- The primary pane and every attached pane are variable width.
- The primary pane remains first and attached panes remain in their explicit horizontal order.
- A visible draggable divider separates every adjacent pair, including primary-to-first-attached.
- Dragging a divider grows one adjacent pane while shrinking the other.
- The primary and attached panes each retain a 320px minimum while enough viewport width exists.
- Attached pane widths remain capped at 960px in persisted state.
- If all requested widths fit, the primary pane consumes any otherwise unused width. The last attached pane therefore ends at the viewport edge with no right gutter.
- If requested widths exceed the viewport after reserving the primary minimum, the attached strip keeps its existing horizontal scrolling behavior.
- Up to six attached panes remain supported; no vertical splitting is introduced.

## Geometry

The App composition root derives geometry once per frame from the central viewport and the bounded attached-width vector.

1. Sum the requested attached widths and internal divider widths with checked, bounded arithmetic over at most six panes.
2. Reserve the attached width requested by state when possible.
3. Give all remaining width to the primary pane.
4. Clamp the primary pane to its 320px minimum. Any attached overflow is represented by the existing horizontal scroll viewport rather than by shrinking panes below their minimum.
5. Set the attached viewport to the exact remaining central width. Its painted background and the final pane cover the full viewport width.

The primary-to-first divider updates the first attachment's stored width. Existing attached-to-attached dividers keep updating the pane immediately to their left. Divider drags remain event-driven and request only the normal follow-up repaint; they add no timer or polling loop.

## Title

The attached header displays only:

```text
Project (Workspace)
```

- `Project` is the final component of the workspace row's configured path.
- `Workspace` is the existing alias-first `workspace_display_name` value.
- If the path has no usable final component, `Project` falls back to the workspace display name.
- Both values are derived from the already loaded bounded workspace rows. Rendering performs no filesystem or Git probe.
- The existing `외부 Pane` prefix and raw pane/session title such as `workspace.spawn.shell 1` are removed.
- Placeholder and live attached headers use the same precomputed title.

## File Ownership

- `crates/app/src/ui/cross_workspace.rs`: expose the existing attached width limits through narrow helpers if the layout calculation needs them; preserve bounded width state.
- `crates/app/src/ui/workspace.rs`: render the supplied precomputed attached title without reconstructing the old external-source/session marker.
- `crates/app/src/app.rs`: derive project/workspace labels, calculate fluid primary/foreign geometry, wire the primary divider, and eliminate the right gutter.

These files are assigned to disjoint subagents. `app.rs` integration starts only after lower helpers and header behavior are reviewed, avoiding overlapping edits.

## Resource Constraints

- Geometry remains O(n) for at most six attachments.
- No new thread, timer, background worker, filesystem access, Git query, or periodic repaint is allowed.
- Divider state reuses existing attachment width storage; no duplicate pane tree or retained frame geometry is introduced.
- Offscreen culling, exact input ownership, one-global cold restore, and the existing runtime visibility rules remain unchanged.

## Validation

Implementation workers may add focused regression tests but defer execution. After integration, run tests once in sequence:

1. Fluid geometry and minimum-width unit tests.
2. Attached header title tests for path, alias, and fallback cases.
3. Existing cross-workspace state, WorkspaceUi, and App integration suites.
4. Full App and runtime tests.
5. `perf-smoke`, boundary check, workspace check, strict Clippy, fmt, and diff-check.
6. Signed macOS package rebuild without relaunch until requested.

Manual acceptance checks the absence of right gutter, primary/first-attached divider dragging in both directions, attached-to-attached resizing, horizontal overflow scrolling, and exact `Project (Workspace)` title rendering.

## Non-Goals

- No vertical splits or recursive split tree.
- No arbitrary two-dimensional pane docking.
- No change to session drag payloads, exact-right insertion ordering, attachment capacity, runtime ownership, or persistence schema.
