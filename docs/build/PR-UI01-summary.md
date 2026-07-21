# PR-UI01 — Pure Connector UI and virtualization

## Outcome

`connector-ui` is now a pure renderer over `connector-contract` snapshots. It emits at most one
`ConnectorIntent` per frame and owns only unfinished form/modal drafts. The crate performs no file,
database, keyring, network, subprocess, parsing, worker, channel, or service work.

Public integration API:

```rust
let mut connector_ui = connector_ui::ConnectorUi::new(catalog);
connector_ui.set_catalog(catalog); // only after a locale change
let intent = connector_ui.render(ui, snapshot);
```

The application owns the current `Arc<ConnectorSnapshot>` and dispatches a returned intent after
the frame. `render` never activates the service by itself. `Activate`, file picker, and external URL
effects are emitted only after explicit user actions.

## Render and resource behavior

- Overview and Slack state read immutable summary fields only.
- Only the selected server can expose a tool page. A missing or mismatched page is never rendered;
  the user action emits `RequestToolPage { server_id, offset }`.
- The renderer borrows the contract's `Arc<[ToolListItem]>` as a slice. It does not clone or rebuild
  the list.
- Server and tool lists use `egui::ScrollArea::show_rows`. The fixed tool viewport renders at most
  the visible range plus egui's one-row overscan on each edge.
- Cached catalog strings are rebuilt only by `set_catalog`, not during ordinary frames.
- No render path builds a `Vec`. The only `Vec` construction is stdio argument/environment material
  created when the user explicitly saves a draft.
- IDs, names, and sensitive input are moved/cloned only when a click produces an intent or opens a
  modal.
- OAuth client-secret and invocation-argument draft types are private, non-Clone,
  non-Serialize, non-Debug, and overwrite their backing string bytes on drop. Intent emission moves
  the bytes into contract `SensitiveInput`.
- JSON file selection and Slack settings opening return `RequestImportPicker` and
  `OpenExternalUrl`; this crate contains no `rfd`, filesystem, or platform command.

## Headless coverage

Seven no-network `egui::Context::run_ui` tests cover:

- AccessKit labels for the server, tool, call action, import picker, and external URL action.
- AccessKit click requests producing file picker and external URL intents rather than platform I/O.
- Explicit selected-server `RequestToolPage` with the correct server and offset.
- Rejection of a tool page belonging to a different selected server.
- A 4,096-tool fixture invoking row rendering only for the six-row viewport plus overscan (maximum
  eight callbacks), while retaining the original `Arc` allocation.
- The one-intent-per-frame slot rule.
- Stdio arguments being allocated into contract vectors only on save.

## Verification

- `cargo test -p connector-ui`: 7 passed, 0 failed; doc-tests 0 failed.
- `cargo check -p connector-ui`: passed.
- `cargo clippy -p connector-ui --all-targets -- -D warnings`: passed.
- `cargo run -q -p xtask -- check-deps`: passed for 23 crates; forbidden edge/cycle count 0.
- `cargo run -q -p xtask -- check-boundary`: passed with the unchanged 53 explicit exceptions.
- `cargo run -q -p xtask -- security-scan`: storage secret persistence 4/4, mcp-store 2/2,
  and audit 37/37 passed; the aggregate then hit the managed sandbox's existing localhost bind
  restriction in 29 MCP HTTP fixtures (`Operation not permitted`), matching the CX00 baseline.
- `cargo fmt --all -- --check`: passed at the milestone.
- `git diff --check`: passed at the milestone.

The first focused compile found one Rust move restriction caused by the OAuth draft's zeroizing
`Drop` implementation; submission now clones only the non-secret server ID on that explicit user
action. The first AccessKit assertion also treated text labels as control labels; the test now
checks both AccessKit `label` and label-role `value`, matching the accessibility protocol.

## Root integration notes

- Construct `ConnectorUi` in `app.rs` only when the Connector surface is first opened; keeping it
  absent before use preserves the zero-resource unopened state.
- Feed it the revision-cached `Arc<ConnectorSnapshot>` from PR-SV01 and dispatch the returned intent
  once after rendering. Do not call the repository/service inside `render`.
- Handle `RequestImportPicker` and `OpenExternalUrl` in the app adapter. Read/parse the selected file
  outside the leaf UI and enforce the contract import byte/item ceilings before service dispatch.
- On locale changes call `set_catalog`; do not rebuild `ConnectorUi` and discard drafts.
- The frozen snapshot has no dedicated "manual OAuth client input requested" state, so this PR
  exposes that draft as an explicit selected-server action. If the service requires automatic
  presentation later, it must use an existing operation/error state or a separately reviewed
  contract revision, not leaf-service coupling.
- Three additional frozen-contract gaps must be resolved before deleting the old Connector UI:
  `RequestImportPicker` has no follow-up intent carrying bounded import data, `ServerSummary` has no
  editable transport config (so URL/stdio edits and enabled toggles cannot be reconstructed), and
  `SlackStatus::NotConfigured` has no server ID or ensure/add intent. Calling service-specific app
  methods for those cases would create a second command path; PR-IN01 must not silently do that.
- Four generic UI words without existing catalog keys (`Refresh`, `Tools`, `Enabled/Disabled`, and
  `Truncated`) currently use stable English fallback text. Before PR-IN01 production cutover, add
  matching keys to every locale and switch the cached labels; do not add an i18n dependency bypass.
