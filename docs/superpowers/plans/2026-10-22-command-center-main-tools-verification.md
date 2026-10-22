# Command Center Main Tools and Verification Implementation Plan

> **For agentic workers:** Use the available native sub-agent orchestration to execute these tasks in isolated worktrees. The user explicitly selected parallel implementation. Root integrates reviewed commits and owns shared host glue.

**Goal:** 프로젝트 메인의 실제 관리 도구·대화를 MAIN에 연결하고, 사용자가 확인한 로컬 검증 명령의 실행 증거를 저장한다.

**Architecture:** 기존 authenticated MCP queue와 host workflow admission을 메인 전용 scope로 확장한다. 실제 structured items/admitted observations를 bounded projection으로 렌더링한다. 검증은 별도 bounded worker와 sidecar evidence에 저장하고 정확한 결과 revision에만 반영한다.

**Tech Stack:** Rust, egui, existing MCP Streamable HTTP, serde, SQLite sidecar, lazy bounded workers, existing native/PWA harness.

---

## Basis / ownership

- Approved scope: preceding user-selected PR1/PR2 and `docs/superpowers/specs/2026-10-22-command-center-main-tools-verification-design.md`.
- Base `1d5db6a7`, integration `feat/agent-command-center-v2-20261020` in `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`.
- PR1 tools: `/private/tmp/deppy-cc-main-tools-20261022`, agent `/root/cc_main_tools`; owns agent-mcp + main-tools helper + launch registration.
- PR2: `/private/tmp/deppy-cc-verification-20261022`, agent `/root/cc_verification`; owns core/storage + verification worker.
- PR1/PR2 UI: `/private/tmp/deppy-cc-conversation-20261022`, agent `/root/cc_conversation_ui`; owns cc DTO/native/PWA rendering.
- Root owns common host integration, observation/data source, shared docs and final review. Agents must not edit root worktree.

## PR1.1 — authenticated main tools

Files: `crates/agent-mcp/src/lib.rs`, `server.rs`, new scoped-tool/auth module/tests; `crates/app/src/command_center_main_tools.rs`, `command_center_launch.rs`.

- [x] Add real protocol RED tests: regular shared/OAuth credential cannot list/call `cc_*`; pending main secret cannot act; exact active main can read own project only; changed generation/config revokes; repeated changed operation rejects.
- [x] Implement main-only Auth registration/lifecycle and credential-specific tools list using the current HTTP request queue.
- [x] Add project/task read, schema-checked plan proposal, confirmed-task dispatch, original-operation lookup through existing host actions.
- [x] Add actual Codex configuration registration test with no model request; append main-only profile/secret env, never worker credentials.
- [x] Root wires scope activation/revocation, bounded request pump and existing current-goal/config/plan admission. Execute actual broker→host→receipt integration test with owned fixture, no live user sessions.
- [x] Run `cargo test --offline --locked -p agent-mcp -- --test-threads=1` and focused `cargo test --offline --locked -p deppy-sijo command_center_main -- --test-threads=1`; archive exact results, commit PR1 tool source.

Implemented API (main credential remains runtime-only):

```rust
MainTools::new(redaction, wake)
MainTools::prepare(PendingMainScope)
MainTools::activate(launch_operation_id, MainScope)
MainTools::reconcile(&current_scopes, &pending_launch_ids)
MainTools::next_request()
MainTools::scope(&request)
```

The local Codex MAIN profile receives a one-use opaque environment credential reference and transient MCP config arguments. The resolver keeps the actual token in memory; no token or ephemeral endpoint enters stored session arguments. Dispatch also carries a revocable local runtime operation permit through actual process creation.

## PR1.2 — actual MAIN conversation

Files: `crates/app/src/ui/command_center/conversation.rs`, `mod.rs`, `main.rs`, native tests; related web view/tests; root new `command_center_conversation.rs`, readonly `ui/agent_sessions.rs` getter and observation hooks.

- [x] Add DTO/render RED tests for exact main route only, changed generation, stable item upsert/order, UTF-8/count/byte caps, missing timestamp and no summary-as-chat.
- [x] Implement `MainConversation`/entry types and native/PWA rendering within approved MAIN layout, retaining composer/worker monitor.
- [x] Root adds exact workspace/session readonly structured getter and projects real `AgentItemKind::UserMessage/AgentMessage/CommandExecution/McpToolCall` items; exclude internal reasoning and unrelated sessions.
- [x] Root records actual admitted CC user prompt and exact completed observed main reply; bind buffer to runtime generation/main route and invalidate on replacement. Tool entries come from actual main request lifecycle.
- [x] Run actual egui/browser/source-adapter tests and inspect offscreen output without launching app. Commit UI/projection source separately from tools.

```rust
Project { main_conversation: Option<MainConversation>, /* existing fields */ }
MainConversation { target, generation, entries, truncated, status }
ConversationEntry { id, turn_id, kind, text, tool_name, state, observed_at }
```

## PR2.1 — runner and durable evidence

Files: `crates/core/src/work_management.rs`, `crates/storage/src/work_management*.rs`, `crates/app/src/work_management/verification.rs`, `verification_worker.rs`, `mod.rs`.

- [x] Add RED tests executing temporary owned shells: success/nonzero/spawn error/timeout/output cap/cancel descendants, exact canonical cwd, stale attempt/result, durable roundtrip and restart-no-replay.
- [x] Implement bounded exact-target DTO and explicit approval object. Reject remote task and cwd outside actual attempt folder.
- [x] Implement worker Running-before-spawn, streaming redaction, capped outputs, child cleanup and terminal-evidence-before-delivery. Reject new work if capacity is full.
- [x] Add additive sidecar schema with verified migration and bounded pages/retention; preserve existing metadata DB bytes.
- [x] Run core/storage and focused worker tests, strict lint; commit this PR2 foundation independently.

```rust
verification::approve_local(&state, &task_id, expected_result_revision, spec)
verification::matches_current(&state, &evidence)
verification::result_state(&evidence)
VerificationWorker::new(data_dir, redaction, wake)
VerificationJob::prepare(state, requested) // pure enqueue; canonicalize/pin only in worker
VerificationJob::Run(approved) // after exact host revalidation of Prepared
VerificationJob::Load { workspace_id, token }
```

## PR2.2 — explicit execute/cancel and exact result application

Files: native/PWA task result view owned by UI agent; root `command_center_verification.rs`, host reducer/pump/projection hooks.

- [x] Add intent/host RED tests: exact task/attempt/result+command/cwd approved, stale revision/config rejected, duplicate operation does not run twice, remote/mobile execution denied, cancelled/interrupted result never marks pass.
- [x] Add native command/cwd/timeout/output-limit form with explicit run/cancel and evidence view. Keep manual result verification visibly separate; PWA receives evidence only.
- [x] Root stages worker request only after current local project/config/attempt checks, tracks original operation and pending owners, cancels invalidated jobs and prevents project eviction while live.
- [x] Root applies Passed/Failed only to exact still-current result after durable evidence delivery. Persist workflow state separately; final confirmation remains explicit.
- [x] Test actual approved request→shell→durable evidence→current reducer outcome and stale/cancel denial with owned temp fixture. Commit PR2 integration.

## Root final review and acceptance

- [x] Independently review all source commits/contracts, cross-agent auth/DTO/storage interfaces and actual RED/GREEN logs. Fix confirmed findings before cherry-pick.
- [x] Integrate PR1/PR2 separately; `git diff` scope check and preserve unrelated work/rollback references.
- [x] Run full affected App/agent-mcp/core/storage suites, relevant browser/native harness, strict affected Clippy and `cargo fmt --all --check`/`git diff --check`. Never claim ignored tests passed.
- [x] Recheck MAIN real data source, provider registration, current-main only authority, output redaction, bounded resources, stale proof and restart-no-replay.
- [x] Update shared handoff, design/plan status, QA report and exact source/evidence hashes. Commit reviewed implementation per PR; no app restart/release/push/main merge.

Final integrated acceptance: product source `587f7099`, App3613PASS/45ignored +other affected packages1504PASS/16ignored; browser265checksPASS; strict allaffected all-target Clippy, fmt and diff checks exit0. Counts exclude nested child duplicates. See `docs/qa/2026-10-22-command-center-pr1-pr2.md` and source/log manifest. No release or restart.
