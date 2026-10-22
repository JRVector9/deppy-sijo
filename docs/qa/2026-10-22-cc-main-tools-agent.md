# PR1 main tools implementation (2026-10-22)

- Worktree: `/private/tmp/deppy-cc-main-tools-20261022`; branch `feat/cc-pr1-tools-20261022`; source base `1d5db6a7`.
- Objective: authenticated project-scoped MAIN management tools, actual Codex MCP registration, preserved exact execution/config/goal identity and existing confirmation/dispatch gates.
- Owned files: `crates/agent-mcp/**`, `crates/app/src/command_center_main_tools.rs`, `crates/app/src/command_center_launch.rs`; root owns host glue and shared handoff.
- Design: independent ephemeral loopback MCP server; a pending, one-time main-only registration injected via verified Codex config flags and secret environment. Auth activation requires exact observed main launch/runtime/session/config. Ordinary cloud sessions never gain management authority. Only project/task read, plan proposal, already-authorized dispatch and original-operation lookup are exposed. No plan confirmation or result/verification bypass tool.
- Local CLI schema verification executed: `codex -c 'mcp_servers.deppy_command_center.url="http://127.0.0.1:18739/mcp"' -c 'mcp_servers.deppy_command_center.bearer_token_env_var="DEPPY_CC_MAIN_TOKEN"' mcp get deppy_command_center --json`; exit 0, streamable_http with bearer-token environment confirmed. No model run or user session input.
- TDD: added `command_center_tools_reject_shared_credentials_before_queue_admission`; RED build running in dedicated target. Result not yet claimed.
- Cargo environment for all commands: `CARGO_TARGET_DIR=/private/tmp/deppy-cc-main-tools-target-20261022 CARGO_BUILD_JOBS=2`.
- Remaining: confirm RED, implement/verify MCP auth+catalog+queue; app bridge and main-only launch registration; root integration, exact lifecycle/host-gate tests.
- No app start/stop/restart, app release, version change, push or main merge.

## MCP foundation checkpoint

- RED command: `cargo test --offline --locked -p agent-mcp command_center_tools_reject_shared_credentials_before_queue_admission -- --exact tests::command_center_tools_reject_shared_credentials_before_queue_admission --nocapture`; exit 101, expected structured authorization rejection absent (`result.isError` was null because tool unknown). No implementation preceded this test.
- GREEN same command: exit 0, 1 passed.
- Additional RED: `cargo test --offline --locked -p agent-mcp command_center_main_initialization_describes_only_authorized_workflow -- --nocapture`; exit 101 because main discovery incorrectly instructed shared-session `notify` instead of management calls.
- GREEN `cargo test --offline --locked -p agent-mcp command_center_ -- --nocapture`; exit 0, all 4 MAIN tests passed (discovery/pending authorization, one-time exact binding, queue-time revocation, shared credential denial).
- Regression `cargo test --offline --locked -p agent-mcp --lib`; exit 0, 96 passed, 1 pre-existing ignored measurement. `cargo fmt -p agent-mcp` executed.
- Failed edit attempt: one apply_patch combined two operations for server.rs and was rejected before changing files; split patch applied successfully.
