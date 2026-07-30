# Pane Drag-and-Drop Feedback Design

## Objective

Make cross-workspace session drag-and-drop discoverable and predictable without
adding idle work or changing source workspace ownership. A user must see which
session is being dragged, which pane will receive the drop, and exactly where
the new foreign pane will be inserted.

This follow-up supersedes the original multi-pane design's far-right-only drop
rule. Hover-button and context-menu opens still append at the far right. A drag
drop inserts directly after the pane under the pointer.

## Approved Visual Behavior

### Context Menu

- Session context menus have a minimum width of 220 px.
- Menu labels use one line and extend the menu instead of wrapping.
- Existing item order, actions, separators, and localization stay unchanged.

### Drag Source

- Every eligible live, stopped, or persisted session row remains draggable.
- While its own payload is active, the source row uses a stronger neutral fill,
  a one-pixel accent border, and a soft elevation shadow.
- The existing status rail remains visible and becomes slightly brighter.
- Layout geometry does not move, so the workspace list does not reflow or
  trigger scroll movement while dragging.
- No terminal title, cwd, scrollback, snapshot, runtime handle, or other large
  presentation data is added to the drag payload.

### Drop Target

- Only visible terminal panes are drop targets.
- The pane under the pointer receives a subtle two-pixel inset outline.
- A three-pixel insertion marker appears on that pane's right edge.
- A compact label communicates that the session will connect on the right.
- Dropping on the primary pane inserts at foreign attachment index zero.
- Dropping on a foreign pane inserts immediately after that attachment.
- Home, Inbox, Fleet, composer, sidebar, offscreen panes, and empty non-terminal
  surfaces do not advertise a valid drop.

### Foreign Workspace Color

- A foreign workspace's identity color is painted only as a one-pixel line at
  the very top of its pane header.
- Header fill, terminal body, separators, resize handle, placeholder body, and
  focus outline use the ordinary DesignALL neutral tokens.
- The line spans only that foreign pane and remains visible for live,
  restoring, suspended, and disconnected states.
- Keyboard focus remains a separate neutral/accent interaction state; it does
  not recolor the foreign pane body.

## Interaction Model

The drop anchor is explicit and bounded.

```rust
enum CrossWorkspaceInsertAnchor {
    Primary,
    Attached(AttachmentId),
    End,
}
```

- Drag release supplies `Primary` or the exact visible `AttachmentId`.
- Hover-button and context-menu actions supply `End`.
- The state layer resolves the anchor to a vector insertion index.
- Missing or stale attached anchors fail closed and do not append elsewhere.
- Exact duplicate targets focus their existing attachment without reordering.
- Capacity rejection leaves all existing order and focus unchanged.

The existing maximum remains `1..=6`. Insertion shifts at most six small
attachment records and creates no additional runtime work compared with the
existing append path.

## Rendering and Data Flow

1. The inactive-workspace session row starts the existing identifier-only drag
   payload.
2. The row renderer checks whether that exact payload is active and paints the
   elevated source treatment.
3. The App computes primary and visible foreign pane rectangles as it already
   does for rendering and culling.
4. Each visible rectangle registers one hover/release interaction carrying its
   `CrossWorkspaceInsertAnchor`.
5. Hover paints only transient geometry; no state is persisted and no repaint
   loop is introduced.
6. Release emits one controller action containing the existing
   `SessionRowTarget` and the exact anchor.
7. Live or cold attachment admission reuses the existing validation, restore
   coordinator, capacity, visibility, and resource-budget paths, differing
   only in the bounded vector insertion index.

## Error and Race Handling

- If an attached pane disappears between hover and release, reject the stale
  anchor instead of silently appending at the end.
- If the dragged target becomes unavailable, preserve the source and show the
  existing unavailable/capacity behavior.
- Releasing outside a valid pane performs no action.
- Foreign header reorder drag payloads and sidebar session payloads remain
  distinct Rust types, so their drop surfaces cannot consume each other.
- Offscreen foreign panes remain unregistered and cannot receive a hidden drop.
- Dropping a cold persisted pane preserves the same one-global-in-flight
  materialization limit and durable restore barrier.

## Resource Contract

- No new thread, timer, channel, queue, polling loop, or periodic repaint.
- No new payload clone beyond the existing small identifier target.
- At most seven transient drop interactions: one primary plus six foreign.
- At most six attachment records shift during insertion.
- Drop highlights are painted only while a matching payload is active and a
  visible pane is hovered.
- Existing offscreen culling, prepare-once/render-many, exact input ownership,
  runtime protection, and warm-budget accounting remain unchanged.

## Test Plan

Follow RED/GREEN cycles in this order:

1. Context-menu style requires at least 220 px and non-wrapping labels.
2. A dragged session row projects elevated fill, border, shadow, and rail boost;
   ordinary hover and unrelated payloads do not.
3. Primary drop resolves to insertion index zero.
4. Attached drop resolves to the exact following index.
5. End anchor preserves hover/context append behavior.
6. Missing attached anchor fails closed.
7. Duplicate focus and capacity rejection preserve existing semantics.
8. Pane hover style paints an outline and right-edge marker only for the
   matching session payload.
9. Foreign pane identity color appears only in the top-line painter and not in
   header/body/resize/placeholder fills.
10. Existing cross-workspace App, file-tree, WorkspaceUi, resource, boundary,
    check, strict Clippy, format, and diff gates remain green.

## Physical Acceptance

- The Korean context menu shown in the reported screenshot stays on one line.
- Dragging a live and a stopped/persisted session visibly lifts the source row.
- Hovering primary and each visible foreign pane highlights only that pane and
  its right insertion edge.
- Dropping on primary inserts first; dropping on a middle foreign pane inserts
  immediately after it.
- Foreign workspace color is visible only as the pane header's top one-pixel
  line.
- Korean IME, Cmd+C, composer input, and host I/O remain routed to the focused
  terminal after insertion.

## Non-Goals

- Vertical insertion or adaptive split placement.
- Dragging from active-workspace rows.
- Persisting attached pane order across app restarts.
- Changing the six-pane cap or runtime admission budget.
- Replacing the retained hover button or context-menu entry.
