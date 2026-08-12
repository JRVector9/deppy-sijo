# Agent Launcher Technical Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show `Opus [1M]` and English reasoning-effort terminology in the Korean launcher without changing execution values or other UI surfaces.

**Architecture:** Keep the existing `ModelChoice` value/label separation and i18n lookup path. Give the exact configured Claude value `opus[1m]` a display-only label during model adoption, and replace only the Korean launcher's shared effort/thinking translations so Claude, Codex, and Kimi inherit the same technical terms.

**Tech Stack:** Rust 2024, existing i18n `Catalog`, Cargo unit tests.

---

### Task 1: Freeze the display contract with failing tests

**Files:**
- Modify: `crates/app/src/agent_launcher.rs`
- Modify: `crates/i18n/src/lib.rs`

- [x] **Step 1: Add the model-label assertion**

Extend `a_configured_model_missing_from_the_catalog_is_still_offered` to assert that the adopted model retains `value() == "opus[1m]"` and exposes `label() == "Opus [1M]"`.

- [x] **Step 2: Add the Korean catalog assertion**

Load `Catalog::load("ko-KR")` and assert the launcher keys resolve to `Reasoning Effort`, `Low`, `Medium`, `High`, `XHigh`, `Max`, `Ultra`, `Thinking`, `On`, and `Off`.

- [x] **Step 3: Verify RED**

Run: `cargo test -p deppy-sijo --locked a_configured_model_missing_from_the_catalog_is_still_offered -- --nocapture`

Expected: FAIL because the current adopted label is `opus[1m]`.

Run: `cargo test -p i18n --locked korean_launcher_keeps_provider_reasoning_terms_in_english -- --nocapture`

Expected: FAIL because the current Korean values are translated.

### Task 2: Apply the minimal display-only changes

**Files:**
- Modify: `crates/app/src/agent_launcher.rs`
- Modify: `crates/i18n/locales/ko-KR/messages.txt`

- [x] **Step 1: Format the exact Claude 1M label**

When `adopt_model` adds `AgentKind::Claude` with exact value `opus[1m]`, pass `Opus [1M]` as the `ModelChoice` label. Preserve the original value for launch arguments.

- [x] **Step 2: Restore English technical terms in Korean launcher copy**

Change only `agent_launcher.thinking`, `agent_launcher.effort`, and `agent_launcher.effort.*` in `ko-KR/messages.txt` to the approved English labels.

- [x] **Step 3: Verify GREEN**

Rerun both Task 1 commands. Expected: both pass.

### Task 3: Integrated verification and handoff

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`

- [x] **Step 1: Run focused and catalog checks**

Run: `cargo test -p deppy-sijo --locked agent_launcher -- --nocapture`, `cargo test -p i18n --locked`, and `cargo run -p xtask --locked -- i18n-check`.

Expected: all pass.

- [x] **Step 2: Run static checks**

Run: `cargo check -p deppy-sijo --all-targets --locked`, `cargo fmt --all -- --check`, and `git diff --check`.

Expected: all pass.

- [x] **Step 3: Record exact results**

Update `docs/CODEX_HANDOFF.md` with modified files, RED/GREEN evidence, command results, any failures, and remaining work. Commit the implementation and documentation.
