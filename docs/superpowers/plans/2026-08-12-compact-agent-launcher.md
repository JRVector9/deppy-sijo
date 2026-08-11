# Compact Agent Launcher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `execute-plan` to implement this plan task-by-task. The user explicitly requested inline implementation without tests.

**Goal:** Replace the current stacked launcher with the approved compact two-column egui modal while preserving all launcher behavior.

**Architecture:** Keep `AgentLauncherUi` as the sole state owner and keep every existing intent/reconciliation path unchanged. Refactor only the modal rendering into a compact header, content-sized left agent list, right option column, and fixed action footer; update localized copy for the approved title and YOLO sentence.

**Tech Stack:** Rust, egui 0.35, repository i18n catalog, macOS app packaging script

---

## File map

- Modify `crates/app/src/ui/agent_launcher.rs`: modal layout, compact sizing constants, agent row presentation, option field layout.
- Modify `crates/i18n/locales/en-US/messages.txt`: title and YOLO help copy.
- Modify `crates/i18n/locales/ko-KR/messages.txt`: `세션` title and approved one-line YOLO help.
- Modify `crates/i18n/locales/ja-JP/messages.txt`: equivalent title and YOLO help.
- Modify `crates/i18n/locales/zh-Hans/messages.txt`: equivalent title and YOLO help.
- Modify `crates/i18n/locales/zh-Hant/messages.txt`: equivalent title and YOLO help.
- Modify `docs/CODEX_HANDOFF.md`: implementation state, changed files, build/package/launch results, and exact next checks.

### Task 1: Update localized launcher copy

**Files:**

- Modify: `crates/i18n/locales/en-US/messages.txt:957-986`
- Modify: `crates/i18n/locales/ko-KR/messages.txt:957-986`
- Modify: `crates/i18n/locales/ja-JP/messages.txt:957-986`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt:957-986`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt:957-986`

- [x] **Step 1: Replace the title in every locale**

Use concise locale equivalents of “Session”:

```text
en-US: agent_launcher.title = Session
ko-KR: agent_launcher.title = 세션
ja-JP: agent_launcher.title = セッション
zh-Hans: agent_launcher.title = 会话
zh-Hant: agent_launcher.title = 工作階段
```

- [x] **Step 2: Make the supported YOLO explanation one sentence**

Use the approved Korean copy and equivalent wording elsewhere:

```text
en-US: Bypass interactive approvals. Enable only when needed.
ko-KR: 대화형 승인을 건너뜁니다. 필요할 때만 활성화하세요.
ja-JP: 対話型の承認を省略します。必要な場合のみ有効にしてください。
zh-Hans: 跳过交互式审批。仅在需要时启用。
zh-Hant: 略過互動式核准。僅在需要時啟用。
```

### Task 2: Build the compact two-column launcher

**Files:**

- Modify: `crates/app/src/ui/agent_launcher.rs:108-333`
- Modify: `crates/app/src/ui/agent_launcher.rs:425-489`

- [x] **Step 1: Add layout constants and content-height calculation**

Add local presentation constants near the UI module top:

```rust
const LAUNCHER_WIDTH: f32 = 620.0;
const AGENT_PANE_WIDTH: f32 = 250.0;
const AGENT_ROW_HEIGHT: f32 = 47.0;
const AGENT_ROW_GAP: f32 = 5.0;
const VISIBLE_AGENT_ROWS: usize = 3;

fn installed_list_height(agent_count: usize) -> f32 {
    let rows = agent_count.min(VISIBLE_AGENT_ROWS);
    if rows == 0 {
        0.0
    } else {
        rows as f32 * AGENT_ROW_HEIGHT + (rows - 1) as f32 * AGENT_ROW_GAP
    }
}
```

This keeps one/two-agent snapshots content-sized, shows three full rows, and scrolls from the fourth.

- [x] **Step 2: Replace the stacked body with two content-sized columns**

In `AgentLauncherUi::show`:

- set both min and max content width to `LAUNCHER_WIDTH`;
- keep the localized title and project subtitle at the top with smaller spacing;
- render the left column at `AGENT_PANE_WIDTH`;
- render the right column from the remaining width;
- capture both column response rectangles and paint one vertical separator after layout, so the separator height equals actual content rather than viewport height;
- keep errors below the body and the two footer buttons below the existing horizontal hairline.

The body closure must continue to call the same `select`, `start_launch`, `Refresh`, `BlankTerminal`, and launch-pending paths so behavior is unchanged.

- [x] **Step 3: Make the installed-agent list compact and bounded**

Render the left header as `Installed agents · N` with detecting state and right-aligned refresh. Use:

```rust
egui::ScrollArea::vertical()
    .id_salt("agent-launcher-installed")
    .max_height(installed_list_height(snapshot.agents().len()))
    .auto_shrink([false, true])
```

Use `AGENT_ROW_GAP` between rows. Preserve the current no-agent empty state when detection finishes with an empty snapshot.

- [x] **Step 4: Render full-width option fields without duplicate headings**

Rewrite `render_options` so it does not draw `실행 설정` or the selected agent identity. For each available option:

```rust
ui.label(catalog.t("agent_launcher.model", &[]));
ui.add_space(4.0);
egui::ComboBox::from_id_salt(("agent-launcher-model", kind.id()))
    .selected_text(selected)
    .width(ui.available_width())
```

Repeat the same vertical field structure for effort/thinking. Keep model-to-effort reconciliation unchanged.

- [x] **Step 5: Compact the YOLO block**

Place a hairline and small vertical gap before the checkbox. Preserve support gating, forced reset for unsupported agents, warning color, and the existing launch option value. Render `agent_launcher.yolo_hint` as one uninterrupted localized string with small weak text.

- [x] **Step 6: Compact the agent rows**

Change `agent_card` to allocate `AGENT_ROW_HEIGHT`, use a 30×30 brand badge, show the display name and command id as two short lines, and paint a selected check at the right edge. Preserve click and double-click response semantics.

- [x] **Step 7: Remove obsolete viewport-height sizing**

Delete `installed_list_max_height`. Retain `combo_popup_height` because it prevents long model and effort dropdowns from being clipped.

### Task 3: Record and launch the implementation

**Files:**

- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Update the handoff after source edits**

Record the objective, exact modified files, preserved behavior, compact sizing decisions, tests intentionally not run, and remaining build/launch work.

- [x] **Step 2: Format only the edited Rust file**

Run:

```bash
rustfmt --edition 2024 crates/app/src/ui/agent_launcher.rs
```

Expected: command exits successfully. This is formatting, not a test.

- [x] **Step 3: Build and package without tests**

Use the repository’s established signed package command:

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' \
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 sh scripts/package-macos.sh
```

Expected: release build completes and `target/bundle/Deppy Sijo.app` is refreshed. Do not describe this as a test pass.

- [x] **Step 4: Replace the running app and open the new bundle**

Resolve the exact running bundle PID, terminate only that process if present, then run:

```bash
SHELL=/bin/zsh open -n 'target/bundle/Deppy Sijo.app'
```

Expected: the new bundle process remains alive and its visible window opens.

- [x] **Step 5: Final handoff update**

Record build/package output, old/new PID evidence, the explicit absence of test execution, and manual visual checks still required.

### Task 4: Apply the approved final launcher refinement

**Files:**

- Modify: `crates/app/src/ui/agent_launcher.rs:5-10`
- Modify: `crates/app/src/ui/agent_launcher.rs:254-366`
- Modify: `crates/app/src/ui/agent_launcher.rs:650-724`
- Modify: `crates/app/src/ui/agent_launcher.rs:820-872`
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Make the panes exactly 50:50 while preserving three visible rows**

Keep `VISIBLE_AGENT_ROWS` at three and derive the pane split from the modal width:

```rust
const LAUNCHER_WIDTH: f32 = 620.0;
const AGENT_PANE_WIDTH: f32 = LAUNCHER_WIDTH / 2.0;
const VISIBLE_AGENT_ROWS: usize = 3;
```

Do not change `installed_list_height`: one through three agents remain content-sized, and the fourth
stays in the existing vertical `ScrollArea`.

- [x] **Step 2: Add a standard close button to the header**

Render the title and a right-aligned frameless `×` in one horizontal row. Use the existing
`action.close` locale key for hover/accessibility text. When clicked and `launch_pending` is false,
set `self.open = false`; disable the button while a launch is pending so it matches backdrop/Escape
behavior.

```rust
let close = egui::Button::new(egui::RichText::new("×").size(18.0)).frame(false);
if ui
    .add_enabled(!self.launch_pending, close)
    .on_hover_text(catalog.t("action.close", &[]))
    .clicked()
{
    self.open = false;
}
```

- [x] **Step 3: Show one centered name per agent row**

Delete the command-id paint call and move the display name from `center_y - 7.0` to the exact row
center. Keep the badge, selection border/check, click, and double-click behavior unchanged.

```rust
ui.painter().text(
    egui::pos2(rect.left() + 47.0, rect.center().y),
    egui::Align2::LEFT_CENTER,
    kind.label(),
    egui::FontId::proportional(14.0),
    visuals.text_color(),
);
```

- [x] **Step 4: Replace the YOLO hint with one persistent safety warning**

For supported agents, render `agent_launcher.yolo_warning` in `palette.warning` directly beneath the
YOLO label. For unsupported agents, keep `agent_launcher.yolo_unsupported`. Remove the conditional
second warning so enabling YOLO never duplicates the text.

```rust
let (message_key, message_color) = if supports_yolo {
    ("agent_launcher.yolo_warning", palette.warning)
} else {
    ("agent_launcher.yolo_unsupported", palette.muted)
};
ui.add(
    egui::Label::new(
        egui::RichText::new(catalog.t(message_key, &[]))
            .size(11.0)
            .color(message_color),
    )
    .wrap_mode(egui::TextWrapMode::Wrap),
);
```

- [x] **Step 5: Format and record the source change without tests**

Run only:

```bash
rustfmt --edition 2024 crates/app/src/ui/agent_launcher.rs
git diff --check
```

Expected: both commands exit successfully. They are formatting/diff checks, not tests. Update
`docs/CODEX_HANDOFF.md` with the final geometry, copy, interaction, and explicit no-test status.

- [x] **Step 6: Package, relaunch, and capture the exact Deppy window**

Run the established signed package command, verify the current bundle PID against the exact executable
path before sending SIGTERM, reopen the bundle with `SHELL=/bin/zsh`, send `Command+T`, resolve the
Deppy CGWindow ID, and capture that window under ignored `target/tmp/launcher-design/`.

Expected visual result: centered compact modal; 50:50 panes; three visible centered-name rows with the
fourth reachable by list scrolling; one persistent safety warning; header `×`; visible footer. Do not
describe build, signing, formatting, or screenshot inspection as a test pass.
