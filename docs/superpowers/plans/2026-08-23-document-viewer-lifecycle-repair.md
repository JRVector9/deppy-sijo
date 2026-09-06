# Document Viewer Lifecycle Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the measured document-viewer state/resource growth, reject oversized saves before cloning, and delete the obsolete editor UI state without changing repaint or same-frame Split Preview behavior.

**Architecture:** Keep the single App-owned `MarkdownViewer`, but separate its slot-stable persisted scroll identity from its revision/theme/width render-cache signature. Make document close the explicit ownership boundary for scroll, CommonMark, link-hook, and image resources. Keep oversized edit buffers intact while making both UI and host save admission use the worker's exact byte boundary before any clone.

**Tech Stack:** Rust 2024, egui 0.35, egui_commonmark 0.24, egui_kittest, existing App document worker and five-locale i18n catalog.

---

## File ownership and parallel execution

- Viewer worker owns only `crates/app/src/ui/markdown_viewer.rs`.
- Save/UI worker owns only `crates/app/src/app.rs` and the five
  `crates/i18n/locales/*/messages.txt` files.
- Review worker is read-only until both edit workers finish.
- Root owns this plan, `docs/CODEX_HANDOFF.md`, integration, formatting, commits, and final review.
- Workers do not commit, format the whole workspace, or edit another worker's files.

This division is intentional: the two production tasks have no shared files. Root runs workspace
formatting only after both diffs are present, so rustfmt cannot create cross-worker noise.

### Task 1: Stable Viewer identities and deterministic cleanup

**Files:**

- Modify: `crates/app/src/ui/markdown_viewer.rs:391-590`
- Test: `crates/app/src/ui/markdown_viewer.rs:610-1040`

- [x] **Step 1: Write failing tests for stable vertical identity and close cleanup**

Add module-private helpers and tests that use real egui persisted state. The production helpers the
tests target have these signatures:

```rust
fn scroll_source_id(slot: MarkdownDocumentSlot) -> egui::Id {
    egui::Id::new(("markdown_viewer_vertical", slot.0))
}

fn scroll_state_id(ui: &egui::Ui, source_id: egui::Id) -> egui::Id {
    ui.make_persistent_id(egui::IdSalt::new(
        egui::Id::new(source_id).with("_scroll_area"),
    ))
}
```

The tests must prove these observable contracts:

```rust
#[test]
fn revision이_바뀌어도_세로_scroll_state_id는_같다() {
    let slot = MarkdownDocumentSlot(7);
    let ctx = egui::Context::default();
    let mut ids = Vec::new();
    let _ = ctx.run_ui(Default::default(), |ui| {
        let source_id = scroll_source_id(slot);
        ids.push(scroll_state_id(ui, source_id));
        ids.push(scroll_state_id(ui, source_id));
    });
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn forget_document는_preview_split의_모든_가로_state와_세로_state를_지운다() {
    let ctx = egui::Context::default();
    let slot = MarkdownDocumentSlot(7);
    let preview = egui::Id::new("preview-horizontal");
    let split = egui::Id::new("split-horizontal");
    let vertical_ids = [
        egui::Id::new("preview-vertical"),
        egui::Id::new("split-vertical"),
    ];
    let mut viewer = MarkdownViewer::new();
    viewer.horizontal_scroll_ids.entry(slot.0).or_default().extend([preview, split]);
    viewer
        .vertical_scroll_ids
        .entry(slot.0)
        .or_default()
        .extend(vertical_ids);
    ctx.data_mut(|data| {
        data.insert_persisted(preview, egui::scroll_area::State::default());
        data.insert_persisted(split, egui::scroll_area::State::default());
        for vertical in vertical_ids {
            data.insert_persisted(vertical, egui::scroll_area::State::default());
        }
    });

    viewer.forget_document(&ctx, slot);

    ctx.data_mut(|data| {
        assert!(data.get_persisted::<egui::scroll_area::State>(preview).is_none());
        assert!(data.get_persisted::<egui::scroll_area::State>(split).is_none());
        for vertical in vertical_ids {
            assert!(data.get_persisted::<egui::scroll_area::State>(vertical).is_none());
        }
    });
}
```

Also render the same slot with two revisions, seed a non-zero vertical offset between frames, and
assert the second revision retains that offset and does not create a second persisted ID.

- [x] **Step 2: Run the new scroll tests and observe RED**

Run:

```bash
cargo test -p deppy-sijo --locked markdown_viewer -- --test-threads=1
```

Expected: the new tests fail because `scroll_source_id`/`scroll_state_id` do not exist,
`horizontal_scroll_ids` stores one ID, and revision is still part of `show_scrollable` identity.

- [x] **Step 3: Separate render signature from persisted vertical identity**

Keep the existing render signature fields, but never pass that signature as the UI identity:

```rust
let source_id = scroll_source_id(view.slot);
let render_key = ScrollCacheKey {
    slot: view.slot.0,
    revision: view.revision.0,
    dark_mode: ui.visuals().dark_mode,
    width_bucket: width_bucket(column_width),
};
if *scroll_key_slot != Some(render_key) {
    if let Some(old_key) = *scroll_key_slot {
        cache.clear_scrollable_with_id(scroll_source_id(MarkdownDocumentSlot(old_key.slot)));
    }
    *scroll_key_slot = Some(render_key);
}

let actual_scroll_state_id = scroll_state_id(ui, source_id);
vertical_scroll_ids
    .entry(view.slot.0)
    .or_default()
    .insert(actual_scroll_state_id);

CommonMarkViewer::new()
    // retain all existing viewer options
    .show_scrollable(source_id, ui, cache, source);
```

The signature invalidates the previous current CommonMark split points/page size. The stable
`source_id` keeps the parent-scoped actual persisted offset across revisions; the per-slot set
retains both Preview and Split parent IDs for close cleanup.

- [x] **Step 4: Track all outer horizontal IDs per slot**

Change the field and insertion to:

```rust
horizontal_scroll_ids:
    std::collections::HashMap<u64, std::collections::HashSet<egui::Id>>,
```

```rust
self.horizontal_scroll_ids
    .entry(destinations_key.0)
    .or_default()
    .insert(scroll_output.id);
```

In `forget_document`, remove the set and delete every `egui::scroll_area::State` it contains.

- [x] **Step 5: Write failing tests for link-hook replacement and image cleanup**

Use the real `MarkdownViewer` and `WorkspaceImageBroker` from the module:

```rust
#[test]
fn link_hooks는_현재_source_집합으로_교체되고_닫으면_비워진다() {
    let workspace = temp_dir("link-hook-replacement");
    let ctx = egui::Context::default();
    let slot = MarkdownDocumentSlot(7);
    let mut viewer = MarkdownViewer::new();
    for (revision, source) in [
        (1, "[A](https://a.example)"),
        (2, "[B](https://b.example)"),
    ] {
        let _ = ctx.run_ui(Default::default(), |ui| {
            viewer.show(
                ui,
                source,
                MarkdownViewerContext {
                    slot,
                    revision: MarkdownSourceRevision(revision),
                    workspace_root: &workspace,
                    base_directory: &workspace,
                },
            );
        });
    }
    assert_eq!(viewer.cache.link_hooks().len(), 1);
    assert!(viewer.cache.link_hooks().contains_key("https://b.example"));
    viewer.forget_document(&ctx, slot);
    assert!(viewer.cache.link_hooks().is_empty());
}

#[test]
fn broker_forget_document는_target_slot의_등록_uri를_즉시_지운다() {
    let workspace = temp_dir("broker-forget-current");
    write_png(&workspace.join("a.png"), 4, 4);
    let ctx = egui::Context::default();
    let slot = MarkdownDocumentSlot(2);
    let mut broker = WorkspaceImageBroker::new();
    broker.sync(
        &ctx,
        &["a.png".to_owned()],
        &MarkdownViewerContext {
            slot,
            revision: MarkdownSourceRevision(1),
            workspace_root: &workspace,
            base_directory: &workspace,
        },
    );
    let uri = broker.registered_uris[0].clone();
    assert!(ctx.try_load_bytes(&uri).is_ok());
    broker.forget_document(&ctx, slot);
    assert!(broker.registered_uris.is_empty());
    assert_eq!(broker.last_generation, None);
    assert!(ctx.try_load_bytes(&uri).is_err());
}

#[test]
fn broker_forget_document는_background_slot으로_active_generation을_지우지_않는다() {
    let workspace = temp_dir("broker-forget-background");
    write_png(&workspace.join("a.png"), 4, 4);
    let ctx = egui::Context::default();
    let active = MarkdownDocumentSlot(2);
    let mut broker = WorkspaceImageBroker::new();
    broker.sync(
        &ctx,
        &["a.png".to_owned()],
        &MarkdownViewerContext {
            slot: active,
            revision: MarkdownSourceRevision(1),
            workspace_root: &workspace,
            base_directory: &workspace,
        },
    );
    broker.forget_document(&ctx, MarkdownDocumentSlot(1));
    assert_eq!(broker.registered_uris.len(), 1);
    assert_eq!(broker.last_generation, Some((active.0, 1)));
}
```

For the same-slot image test, query the registered URI through egui before and after close so the
test verifies context bytes/image/texture eviction, not only the broker vector.

- [x] **Step 6: Run the resource tests and observe RED**

Run the same focused command. Expected: historical link A remains and same-slot image URI remains
registered because neither current hook replacement nor close-time broker cleanup exists.

- [x] **Step 7: Implement current-hook replacement and slot-aware broker cleanup**

Before registering current links:

```rust
cache.link_hooks_clear();
for dest in link_targets {
    cache.add_link_hook(dest.clone());
}
```

Add broker cleanup:

```rust
fn forget_document(&mut self, ctx: &egui::Context, slot: MarkdownDocumentSlot) {
    if self.last_generation.is_none_or(|generation| generation.0 != slot.0) {
        return;
    }
    for uri in self.registered_uris.drain(..) {
        ctx.forget_image(&uri);
    }
    self.last_generation = None;
}
```

Complete `MarkdownViewer::forget_document` so target slot scroll states are always removed, while
the singleton render signature, destinations, hooks, and broker are cleared only when their current
key/generation belongs to the target slot. Calling it twice must remain a no-op.

- [x] **Step 8: Run Viewer tests to GREEN**

Run:

```bash
cargo test -p deppy-sijo --locked markdown_viewer -- --test-threads=1
```

Expected: all existing and new markdown viewer tests pass; no test count is claimed until the
command actually finishes.

### Task 2: Save admission boundary, overflow UX, and dead state

**Files:**

- Modify: `crates/app/src/app.rs:9025-9360,15740-16175,16340-16480,28340-28410`
- Modify: `crates/i18n/locales/en-US/messages.txt`
- Modify: `crates/i18n/locales/ja-JP/messages.txt`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt`
- Test: `crates/app/src/app.rs:34320-35190`

- [x] **Step 1: Write failing exact-boundary and admission-order tests**

Add a base eligibility helper and tests around the existing `stub_open_document`:

```rust
#[test]
fn document_can_save는_8mib까지_허용하고_그_다음_byte부터_막는다() {
    let dir = unique_temp_dir("save-limit-app");
    let path = dir.join("a.md");
    std::fs::write(&path, b"x").unwrap();
    let load = document_io::load_document(&document_io::DocumentLoadRequest {
        path: path.clone(),
    });
    let document_io::DocumentLoadOutcome::Loaded { revision, .. } = load else {
        panic!("expected Loaded");
    };
    let mut document = stub_open_document("/tmp/a.md", "x", "", true);
    document.load_state = DocumentLoadState::Loaded {
        revision,
        limit: document_io::DocumentLimitTier::Full,
    };
    document.source = "x".repeat(document_io::DOCUMENT_REFUSE_BYTES_MAX as usize);
    assert!(document.can_save());
    document.source.push('x');
    assert!(!document.can_save());
    document.source.pop();
    assert!(document.can_save());
    std::fs::remove_dir_all(dir).unwrap();
}
```

Add a source-order regression that extracts only
`request_document_save` and asserts the `DOCUMENT_REFUSE_BYTES_MAX` guard occurs before its first
`document.source.clone()` and that overflow removes `document_close_after_save`.

- [x] **Step 2: Run the save tests and observe RED**

Run:

```bash
cargo test -p deppy-sijo --locked document -- --test-threads=1
```

Expected: the exact +1 byte `can_save` assertion fails and the request function has no pre-clone
size guard.

- [x] **Step 3: Add one shared App-side save eligibility contract**

Implement the boundary with the worker's public constant:

```rust
fn has_save_eligibility(&self) -> bool {
    self.dirty && !self.saving && self.is_editable()
}

fn source_fits_save_limit(&self) -> bool {
    self.source.len() as u64 <= document_io::DOCUMENT_REFUSE_BYTES_MAX
}

fn can_save(&self) -> bool {
    self.has_save_eligibility() && self.source_fits_save_limit()
}

fn can_save_then_close(&self) -> bool {
    self.dirty && self.is_editable() && self.source_fits_save_limit()
}
```

In `request_document_save`, check `has_save_eligibility` first and `source_fits_save_limit`
second. On overflow set `ContentTooLarge`, remove the close-after-save continuation, and return
before either source clone. Preserve the current two owned snapshots for accepted async saves.

- [x] **Step 4: Add explicit overflow UI in toolbar and close confirmation**

Before the existing grew-past-Full status, return a new localized save-limit status whenever the
editable source exceeds 8 MiB. In the dirty-close modal compute the action eligibility from the
target document. It matches `can_save` except that an already-running save may install the existing
close-after-save continuation without starting a second request; the source must still be dirty,
editable, and within the byte limit. Show the same explanation when overflowed, and render Save with:

```rust
ui.add_enabled(
    can_save_then_close,
    egui::Button::new(text.t("document.confirm_discard.save", &[])),
)
```

Keep Discard and Cancel enabled. Add this key to all five locale catalogs:

```text
document.limit.save_too_large
```

The message must say that the buffer is preserved, cannot be saved above 8 MiB, and becomes
saveable again after reducing its size. Do not expose document content or path.

When an in-flight save produces `SaveConflict`, replace a same-document pending
`CloseWithDirty` at its current queue position instead of dropping the conflict. On save-worker
spawn/panic/disconnect failure, clear `saving`, `saving_source`, and the close-after-save
continuation together. Preserve other documents' FIFO ordering.

- [x] **Step 5: Remove the obsolete `body_ui_id` path**

Delete the field, render-time `ui.id()` capture/write, both initializers, and comments that claim
cleanup depends on a body container ID. Keep the absolute editor identity and cleanup unchanged:

```rust
fn document_source_editor_id(path: &Path) -> egui::Id {
    egui::Id::new(("document_tab_source_editor", path))
}

fn clear_document_editor_state(ctx: &egui::Context, document: &OpenDocument) {
    let editor_id = document_source_editor_id(&document.path);
    ctx.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(editor_id));
}
```

Add a production-source assertion that `body_ui_id` has zero occurrences.

- [x] **Step 6: Run save/UI/i18n tests to GREEN**

Run:

```bash
cargo test -p deppy-sijo --locked document -- --test-threads=1
cargo run -p xtask --locked -- i18n-check
```

Expected: focused document tests and all i18n checks pass.

### Task 3: Integration, adversarial review, and measured handoff

**Files:**

- Modify: `docs/CODEX_HANDOFF.md`
- Verify: every file changed since `9128dad`

- [x] **Step 1: Inspect ownership and formatting boundaries**

Run:

```bash
git status --short
git diff --name-only 9128dad
git diff --check
```

Expected changed production ownership: Viewer worker only `markdown_viewer.rs`; Save/UI worker only
`app.rs` and five locale files. Root-owned plan/handoff changes are allowed.

- [x] **Step 2: Run rustfmt once at the integration boundary**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
```

Expected: both checks exit 0.

- [x] **Step 3: Run focused and architecture gates**

Run:

```bash
cargo test -p deppy-sijo --locked document -- --test-threads=1
cargo test -p deppy-sijo --locked markdown_viewer -- --test-threads=1
cargo run -p xtask --locked -- i18n-check
cargo run -p xtask --locked -- check-boundary
```

Expected: every command exits 0. Record exact pass/ignore counts from output.

- [x] **Step 4: Run strict compile-quality gates**

Run:

```bash
cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0 with no warning promoted to an error.

- [x] **Step 5: Perform two-stage final review**

First review spec compliance: stable offset, bounded state/hook counts, owner-safe close cleanup,
exact save boundary, continuation handling, dead-field removal, and no repaint changes. Then review
code quality: borrow/lifetime safety, hidden full-source work, logging privacy, test realism, and
unrelated diff. Reject any finding that cannot cite a changed line and a concrete failing scenario.

- [x] **Step 6: Update the handoff with evidence**

Replace the current task section in `docs/CODEX_HANDOFF.md` with the implemented objective,
completed work, modified files, design decisions, exact commands/results, failed attempts, remaining
work, and exact next commands. Never claim a gate passed without its completed output.

- [x] **Step 7: Create the integration commit**

After review findings are fixed and all gates are green:

```bash
git add crates/app/src/app.rs crates/app/src/ui/markdown_viewer.rs \
  crates/i18n/locales/en-US/messages.txt crates/i18n/locales/ja-JP/messages.txt \
  crates/i18n/locales/ko-KR/messages.txt crates/i18n/locales/zh-Hans/messages.txt \
  crates/i18n/locales/zh-Hant/messages.txt docs/CODEX_HANDOFF.md \
  docs/superpowers/plans/2026-08-23-document-viewer-lifecycle-repair.md
git commit -m "fix(app): bound document viewer lifecycle state"
```

Expected: commit succeeds; `git status --short --branch` is clean and local `main` is ahead of
`origin/main` by the documentation commit plus the implementation commit. Do not push unless the
user asks.
