# Partial Backlog Code Triage

작성일: 2026-07-05

## Scope

This triage re-checks the partial/backlog items after PR-U13 and PR-U21 through
PR-U26 were completed. The goal is to separate code work that should be
implemented from items that are intentionally deferred or already covered.

Checked command:

- `cargo run -p xtask -- check-deps` - pass, no forbidden edge/cycle.

## PR-U26 Consistency Check

Verdict: current.

`docs/build/PR-U26-terminal-dirty-range-partial-render.md` matches the current
implementation. The code has:

- session dirty row accumulation and `TerminalViewportSnapshot.dirty_ranges`:
  `crates/session/src/session.rs`
- per-session `TerminalRenderCache` and dirty-row cache rebuild tests:
  `crates/terminal/src/renderer_egui.rs`
- workspace-side render cache ownership and hidden cache drop:
  `crates/app/src/ui/workspace.rs`
- remote delta dirty range restoration and keyframe full-dirty promotion:
  `crates/runtime/src/remote.rs`

No additional PR-U26 update is needed from this pass.

## Implement Next

### PR-U05b - Git Ignore Matcher Expansion

Classification: implement.

Evidence:

- `FileTreeUi` now has nonrecursive watcher registration, debounce batching,
  caller-provided ignore prefixes, hidden filtering, and a fixed generated-file
  ignore list.
- `crates/app/src/ui/file_tree.rs` still uses `default_watch_ignore_name`
  rather than parsed `.gitignore`, `.git/info/exclude`, or global git excludes.
- No `ignore` or `globset` dependency is present in workspace manifests.

Implementation scope:

- Add a cached ignore matcher per workspace root.
- Apply it consistently to directory listing, flattening, and watcher events.
- Cover root `.gitignore`, nested `.gitignore`, `.git/info/exclude`, and global
  git excludes where discoverable.
- Keep `.env*` warning signaling and existing hidden-file toggle behavior.

Tests:

- Ignored file/dir is absent from folder tree and watcher dirty events.
- `.env` warning still fires when not ignored.
- Paths with spaces/CJK/emoji still insert into terminal without auto-enter.

### PR-U06b - Terminal Internal Selected-Text DnD Contract

Classification: implement.

Evidence:

- Terminal selection and copy exist in `crates/app/src/ui/workspace.rs`.
- Terminal drop handling currently consumes `PathBuf` payloads from the folder
  tree and writes paste bytes to `RuntimeClient`.
- There is no separate selected-text drag payload contract for dragging terminal
  selected text back into a terminal pane.

Implementation scope:

- Add a terminal-selected-text DnD payload type that is distinct from `PathBuf`.
- Dropping selected text onto a terminal pane should paste through the same
  bracketed paste helper as clipboard/path paste.
- Preserve no-auto-enter behavior.
- Do not start a new terminal selection while any DnD payload is active.

Tests:

- Selected text DnD pastes exact bytes with and without bracketed paste.
- CJK wide chars and emoji text remain intact.
- File tree `PathBuf` DnD still routes through path quoting and does not
  conflict with text payloads.

### PR-U09b - Production Profile Confirmation Guard

Classification: implement.

Evidence:

- `crates/app/src/ui/agents.rs` shows production profile labels and warning
  text when a production profile is selected.
- `AgentsUi::run` still sends `RuntimeCommand::SpawnAgent` directly once the
  user clicks run.

Implementation scope:

- Add a confirmation modal/state before spawning with a production env profile.
- Confirmation must identify the profile and agent but must not display secret
  values.
- Non-production profiles should continue spawning without an extra prompt.

Tests:

- Production profile click opens confirmation and does not send `SpawnAgent`.
- Confirm sends exactly one `SpawnAgent`.
- Cancel leaves runtime untouched.

### PR-U10b - Scoped MCP Env Injection

Classification: implemented in PR-U10b.

Evidence:

- `mcp::McpServerConfig` contains only `name`, `command`, and `args`.
- `StdioClient::spawn` builds `Command` without a scoped env map.
- `mcp-proxy` reconstructs `McpServerConfig` from stored server rows with only
  command/args.

Implementation scope:

- Add an explicit scoped env representation for local MCP server spawn.
- Store only env keys and credential ids or non-secret plain values according to
  the existing env safety rules.
- Resolve credential-backed env values immediately before spawning the MCP
  server, register resolved secrets for redaction, and never persist or log
  secret values.
- Keep UI leaf code behind existing service/runtime boundaries where practical.

Tests:

- MCP server receives only scoped env values, not the full parent environment
  when strict scoped mode is enabled.
- Credential-backed env values are resolved at spawn and redacted from stderr,
  audit previews, and error strings.
- Stored rows contain credential ids or safe plain values only.

Result:

- `mcp_servers` now stores safe plain env and credential ids plus
  `inherit_env`.
- `StdioClient::spawn` applies explicit scoped env and supports strict
  `env_clear` mode.
- Connector Center and `deppy-mcp-proxy` resolve credential env through the
  secret store immediately before backend spawn and register resolved values for
  redaction.

### PR-U20b - Full Scenario A-E Performance Report

Classification: implement as release-gate measurement, not app-code hardening.

Evidence:

- `docs/build/PR-U20-summary.md` explicitly marks full Scenario A-E RSS/CPU/p95
  measurements and `docs/performance/final-gate.md` as pending.
- `crates/app/src/perf.rs` has the hidden-session load harness and frame p95
  logging hooks, but no final measurement report exists.

Implementation scope:

- Create `docs/performance/final-gate.md`.
- Run the planned scenarios with frame p95, CPU, RSS, queue/backpressure, and
  notes for test hardware/build profile.
- Treat failures as targeted follow-up PRs rather than broad refactors.

## Defer / No Code Now

### PR-U02 - Full Store-Crate Explosion

Classification: defer.

Reason:

- `cargo run -p xtask -- check-deps` passes with no forbidden edge/cycle.
- The v2.8 reality addendum intentionally accepts the current DAG and defers the
  full `audit-store`/`mux-store`/`env-store`/`session-store` split unless a new
  cycle or forbidden edge appears.

Trigger to reopen:

- New `mcp`/`audit`/`persist`/`storage` cycle.
- `storage-core` depending on domain/runtime crates.
- Store logic moving back into runtime crates.

### PR-U06 - Clipboard Failure Abstraction

Classification: defer unless product requires explicit clipboard failure UI.

Reason:

- Current copy path uses `egui::Context::copy_text`, which does not expose a
  synchronous failure result to the caller.
- Adding reliable failure reporting would require introducing a platform
  clipboard service and replacing the egui output path, which is larger than the
  current terminal UX hardening need.

Trigger to reopen:

- A concrete platform failure mode is reproduced.
- The product requires visible clipboard failure notifications.

### PR-U10 - Schema Reapproval Cache

Classification: already covered for current architecture.

Reason:

- `audit::PermissionPolicy` binds `Allow` rules to an approved schema hash and
  returns `SchemaChanged` when the request hash differs.
- Connector UI prepares the live/current schema hash before policy evaluation.
- `mcp-proxy` discovers live schema hashes per proxy session, caches them for
  that session, and fail-closes to approval when it cannot confirm a matching
  hash.

Remaining note:

- Mid-session backend spec swap cache invalidation is intentionally not handled;
  a new proxy session re-discovers the backend schema.
