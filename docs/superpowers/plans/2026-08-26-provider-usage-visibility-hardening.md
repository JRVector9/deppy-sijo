# Provider Usage Visibility Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep every installed and enabled provider visibly represented while making Claude and Kimi usage probes use the exact launcher-detected executable and PATH, stop disabled probes, reject lost PTY input, and preserve provider cells at narrow widths.

**Architecture:** App combines launcher detection and disabled configuration into provider visibility, while usage modules return only fresh numeric data. Kimi carries the same detected-versus-unavailable distinction as Grok. Claude and Kimi receive `DetectedAgent` from App, build bounded PTY commands from its absolute executable and launch PATH, and fail explicitly when required input is not accepted.

**Tech Stack:** Rust 2024, egui 0.35, egui_kittest, portable-pty wrapper crate, standard-library bounded channels, Cargo tests and Clippy.

---

## File map

- Modify `crates/app/src/app.rs`: construct detected/enabled Kimi state, gate Claude/Kimi probes, preserve Codex merge, and expose pure provider visibility/layout helpers for tests.
- Modify `crates/app/src/kimi_usage.rs`: accept launcher detection, reuse its executable/PATH, validate PTY input, retain bounded cache semantics, and update the live regression.
- Modify `crates/app/src/claude_usage.rs`: accept launcher detection, reuse its executable/PATH, validate PTY input, and add parser/command tests.
- Modify `crates/app/src/ui/agent_terminal.rs`: accept Kimi detected/no-value state, retain its placeholder, and compact provider rendering against real available width.
- Modify `docs/CODEX_HANDOFF.md`: record RED/GREEN commands, live results, review corrections, and remaining delivery work.

### Task 1: Reproduce and classify the current Kimi failure

**Files:**
- Modify: `crates/app/src/kimi_usage.rs:271`

- [x] **Step 1: Add a bounded ignored diagnostic test**

Add a test-only live case that runs the launcher-detected Kimi executable and returns only typed usage, never raw PTY output:

```rust
#[test]
#[ignore = "installed Kimi CLI diagnostic"]
fn kimi_실측_프로브는_런처_감지_경로로_끝난다() {
    let snapshot = crate::agent_launcher::detect_installed_agents(
        crate::agent_shim::shim_path().as_deref(),
    );
    let agent = snapshot.find(crate::agent_launcher::AgentKind::Kimi)
        .expect("installed Kimi must be detected");
    let usage = fetch_kimi_usage(agent).expect("bounded Kimi probe");
    assert!(usage.is_some(), "installed account produced no typed usage");
}
```

- [x] **Step 2: Run the live test and preserve the expected RED**

Run:

```bash
env -i HOME=/Users/jr USER=jr LOGNAME=jr SHELL=/bin/zsh \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin \
  __CFBundleIdentifier=app.vector9.deppy-sijo \
  CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 \
  /Users/jr/.cargo/bin/cargo test -p deppy-sijo --bin deppy-sijo --locked \
  kimi_실측_프로브는_런처_감지_경로로_끝난다 -- --ignored --nocapture
```

Expected: FAIL because Kimi 0.38.0 returns no typed usage. Record whether the probe reached a terminal no-data state or timed out without logging raw output.

- [x] **Step 3: Compare with the working Grok pattern**

Read all of `crates/app/src/grok_usage.rs` and verify the relevant differences: launcher-owned executable/PATH, required-input acceptance, one total deadline, completed-at timestamp, and detected/no-value UI state.

### Task 2: Preserve the Kimi cell through unavailable states

**Files:**
- Modify: `crates/app/src/app.rs:7383-7605`
- Modify: `crates/app/src/ui/agent_terminal.rs:295-356`
- Test: `crates/app/src/ui/agent_terminal.rs:1418-1597`

- [x] **Step 1: Write the Kimi placeholder RED test**

Change the status-bar test helper's Kimi input to `Option<Option<ProviderUsage>>` and assert:

```rust
let harness = run(None, some_usage, Some(None), None, Vec::new());
assert!(harness.query_by_label("Kimi logo").is_some());
harness.get_by_label("—");
```

Also preserve explicit cases for `None` (undetected) and disabled (hidden).

- [x] **Step 2: Run the focused test and verify RED**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked \
  kittest_사용량_바_칸은_켜짐_값없음과_꺼짐을_구분해_그린다 -- --nocapture
```

Expected: compile or assertion failure because Kimi currently accepts only one `Option` and hides no-value state.

- [x] **Step 3: Implement the minimal detected/no-value boundary**

Use the same bounded representation as Grok at the App-to-status boundary:

```rust
pub(crate) struct ProviderUsageInputs<'a> {
    pub(crate) claude: Option<ProviderUsage>,
    pub(crate) codex: Option<ProviderUsage>,
    pub(crate) codex_meta: Option<&'a CodexUsageMeta>,
    pub(crate) kimi: Option<Option<ProviderUsage>>,
    pub(crate) grok: Option<Option<GrokUsage>>,
}
```

`Some(None)` is detected but unavailable, `Some(Some(value))` is numeric, and `None` is undetected. Disabled configuration still wins.

- [x] **Step 4: Run the focused UI test and verify GREEN**

Run the command from Step 2. Expected: PASS.

### Task 3: Make Kimi and Claude probes use launcher detection and accepted input

**Files:**
- Modify: `crates/app/src/kimi_usage.rs:51-185`
- Modify: `crates/app/src/claude_usage.rs:22-152`
- Test: both files' `#[cfg(test)]` modules

- [ ] **Step 1: Write command-construction RED tests**

For each provider, create a pure command builder and assert the launcher path wins:

```rust
let command = kimi_probe_command(
    Path::new("/custom/bin/kimi"),
    PathBuf::from("/tmp/probe"),
    Some(OsStr::new("/custom/bin:/usr/bin:/bin")),
).unwrap();
assert_eq!(command.program, "/custom/bin/kimi");
assert!(command.env.iter().any(|(k, v)| k == "PATH" && v.starts_with("/custom/bin:")));
```

Repeat for Claude. Add a pure assertion that a non-accepted `PtyInputEnqueueResult` is rejected.

- [ ] **Step 2: Run both module test groups and verify RED**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked kimi_usage::tests:: -- --nocapture
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked claude_usage::tests:: -- --nocapture
```

Expected: FAIL because the desired builders/signatures do not exist and Claude currently has no module tests.

- [ ] **Step 3: Implement launcher-owned command construction**

Change both entry points to:

```rust
pub(crate) fn current(
    ctx: &egui::Context,
    agent: Option<&crate::agent_launcher::DetectedAgent>,
) -> Option<ProviderUsage>
```

Clone the detected executable and launch PATH only when probe admission succeeds. Build `CommandSpec` with absolute `program`, `TERM=xterm-256color`, and the detected bounded `PATH` when present. Delete the duplicate `resolve_*_command` and `*_command_path` functions.

- [ ] **Step 4: Validate required input**

Add provider-local helpers matching Grok's contract:

```rust
fn write_required_input(session: &mut dyn pty::PtySession, bytes: &[u8]) -> anyhow::Result<()> {
    let result = session.write_input(bytes)?;
    anyhow::ensure!(result.is_accepted(), "usage PTY input was not accepted");
    Ok(())
}
```

Use this for slash commands and trust/menu confirmation. Do not silently discard backpressure or closed-session results.

Also record `last_request` for a failed worker-thread spawn, matching Grok, so a resource-exhaustion failure cannot retry on every frame. Pin this with a pure state/helper test.

- [ ] **Step 5: Run both module groups and verify GREEN**

Run the commands from Step 2. Expected: all non-live tests pass, live tests remain explicitly ignored.

### Task 4: Stop disabled probes and supply detected Kimi state

**Files:**
- Modify: `crates/app/src/app.rs:26992-27039`
- Test: `crates/app/src/app.rs` source-law/pure tests

- [ ] **Step 1: Write probe-admission RED tests**

Extract and test a pure gate:

```rust
assert!(!provider_probe_enabled(&["kimi".into()], AgentKind::Kimi, true));
assert!(!provider_probe_enabled(&[], AgentKind::Kimi, false));
assert!(provider_probe_enabled(&[], AgentKind::Kimi, true));
```

Add a source-law assertion that `kimi_usage::current` and `claude_usage::current` occur only behind this gate and receive the launcher-detected agent.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked provider_probe_enabled -- --nocapture
```

Expected: FAIL because the gate does not exist and App calls Kimi unconditionally.

- [ ] **Step 3: Implement App gating**

Resolve `claude_agent`, `kimi_agent`, and `grok_agent` once from `agent_launcher_snapshot`. Call Claude/Kimi PTY fallbacks only when the corresponding agent is detected and enabled. Construct Kimi status as `kimi_agent.map(|_| kimi_usage)` so detected no-value remains visible.

- [ ] **Step 4: Run focused App and status tests and verify GREEN**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked provider_probe_enabled -- --nocapture
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo --locked ui::agent_terminal::tests:: -- --nocapture
```

Expected: PASS.

### Task 5: Preserve provider visibility at narrow widths

**Files:**
- Modify: `crates/app/src/app.rs:7328-7605`
- Test: `crates/app/src/ui/agent_terminal.rs:1418-1597`

- [ ] **Step 1: Write the narrow-width RED test**

Render all four detected/available providers at widths 300, 430, 620, 810, and 1,200. For every width assert that each provider accessibility node intersects the root clip rect and that Kimi's right edge does not exceed it.

- [ ] **Step 2: Run and verify RED**

Run the focused status-bar test. Expected: at least the 300pt case fails because Kimi is drawn last in an overflowing single row.

- [ ] **Step 3: Implement deterministic compact modes**

Compute a compact level from `ui.available_width()` and visible provider count. Preserve logo plus one numeric/placeholder label for every provider first. Show 5h bar, secondary window label, and plan label only as space permits. Keep one line and do not add a timer or horizontal scroll.

- [ ] **Step 4: Run the width sweep and status UI group**

Expected: all provider nodes remain inside the clip rect at every tested width and the existing wide layout/order assertions pass.

### Task 6: Live verification, complete gates, review, and delivery record

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Create: Obsidian journal under `프로젝트 일지/deppy-sijo/`

- [ ] **Step 1: Run live provider regressions**

Run launcher-detected Kimi and Grok tests in the Finder-minimal environment. The Kimi diagnostic may report a typed unavailable state when the installed account does not expose managed plan data; require numeric usage only when `DEPPY_KIMI_EXPECT_USAGE=1` is explicitly set. In both cases the persistent UI placeholder must pass without inventing numbers. Grok must remain numeric.

- [ ] **Step 2: Run focused and repository gates**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --locked
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo run --locked -p xtask -- i18n-check
cargo run --locked -p xtask -- check-boundary
```

- [ ] **Step 3: Run independent Codex review**

Review only changed Rust source with `codex review --uncommitted`. Apply every Critical/High and relevant Medium issue, then rerun affected tests.

- [ ] **Step 4: Update handoff and commit**

Record exact commands/results in `docs/CODEX_HANDOFF.md`, create the Workstep Obsidian journal, and commit the implementation with a Korean Conventional Commit message. Do not push, package, replace the Desktop bundle, or relaunch without an explicit user request.
