# Automatic MCP Connection Implementation Plan

> **For agentic workers:** Execute inline with execute-plan and TDD; user has requested implementation and corrected the previous review-only interpretation. No additional execution approval or subagent dispatch is needed.

**Goal:** MCP 연결하기 creates and manages a public MCP URL without requiring the user to install Tailscale/cloudflared, create a Cloudflare account, or keep a terminal window open.

**Architecture:** Ship a pinned, verified cloudflared companion in the app. Start the existing loopback MCP server, launch the companion in an owned worker, accept only its strict trycloudflare HTTPS hostname and synchronize the actual server Host/Origin/OAuth base, then verify public protected-resource metadata before reporting ready. Retain manual fixed-host mode as an advanced option. Preserve existing session authorization, PTY receipts, read_output and notify. There is no new LLM, background terminal session, command replay, or deployed Deppy gateway in this unit.

**Tech Stack:** Rust std processes/threads/channels, existing ureq/egui/serde, Python stdlib build-time artifact preparation, Developer ID macOS signing.

## Scope and release

- Default automatic temporary mode works on macOS with the companion included. No external user install/account. Cloudflare service reachability is still necessary; manual fixed HTTPS mode remains available when another tunnel/service is used. A provider-independent operated fixed gateway is separate infrastructure, not implicitly created here.
- Preserve manual host/port coordinates (already persisted by App config; correct the earlier report's inference from constructor defaults). Persist the automatic/manual choice; do not persist temporary URL as a manual host or restore input permission.
- Keep auto connection explicit on button click. Normal stop and app exit revoke MCP access and stop/reap only the owned helper. Startup cancellation and duplicate clicks must not spawn two live helpers.
- Version 0.1.0 → 0.2.0 before final release build. Update workspace lockfile entries and verify compiled/package version inputs. Build only; no GUI launch/restart in this task.

### Task 1: Verified bundled companion

Files: create `scripts/prepare-cloudflared.py`, `scripts/tests/test_prepare_cloudflared.py`; modify `scripts/dev-run.sh`, `scripts/package-macos.sh`, `scripts/verify-macos-package.sh`.

- [x] RED: build-preparation tests require rejecting a wrong SHA256 before extraction and accepting only a regular archive member named cloudflared; missing files/link members cannot be extracted.
- [x] GREEN: implement pinned 2026.9.1 Darwin arm64/amd64 archive preparation with digest checked against GitHub release assets. Write helper beside the app/proxy as `deppy-cloudflared`; keep cached source archive under target, never use Homebrew or user-global installation. Include upstream license notice.
- [x] Verify `python3 -m unittest discover -s scripts/tests -p test_prepare_cloudflared.py`; prepare release and debug helpers; execute helper `--version` (no tunnel yet).
- [x] Integrate preparation and signing into dev/package scripts. Production packaging signs the helper before signing the app bundle; package verification checks helper signature and expected bundle/app version.

Pinned official asset digests:

```text
cloudflared-darwin-arm64.tgz c27ab8fd0aa489449e3d201eb02f957ef460a13b613662928b1b23394bf1bcfe
cloudflared-darwin-amd64.tgz ff0d3b51d5ff70eceef89d6b32145fee985018a2174596a5dbe405e2766e2ac4
```

### Task 2: Owned tunnel and MCP hostname readiness

Files: create `crates/app/src/cloud_agent/tunnel.rs`; modify `crates/agent-mcp/src/server.rs` and its tests.

- [x] RED: parser accepts `https://abc-123.trycloudflare.com` from bounded helper output and rejects spoofed suffix, credentials, path/query, HTTP and oversized labels. Server metadata must reflect a newly accepted public host and reject invalid host updates.
- [x] GREEN: dedicated child process (`stdin/stdout` null, continuously drained bounded stderr), cancellation flag, bounded worker event slot, exact child kill/reap and temporary private empty config. Disable companion auto-updates. Update server public-host snapshot used by real request validation/metadata.
- [x] Validate readiness by reading public protected-resource metadata with a bounded request timeout and checking the exact resource URL. URL discovery alone never marks the connection ready. Timeout/exit/stop emit bounded statuses, never raw helper logs/credentials.
- [x] Verify parser, lifecycle (real fixture child, oversized log, timeout, cancellation, unexpected exit), and server Host/metadata tests. Commands: `cargo test -p agent-mcp`; `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent::tunnel`.

### Task 3: App/UI/config and actual MCP flow

Files: `cloud_agent.rs`, `cloud_agent/ui.rs`, `app.rs`, `config.rs`, five `crates/i18n/locales/*/messages.txt`.

- [x] RED: existing default start has no automatic worker/connecting state; UI start/cancel, config choice roundtrip and hostname/token-generation behavior must be covered by headless tests.
- [x] GREEN: default automatic choice; manual advanced mode; preparing/connecting/ready/stopping/failed statuses; URL copying only after verification, cancellation during startup, duplicate-click guard, failure revokes queued input. Actual shutdown stops the helper before runtime teardown. Respect existing consent/operation_id/generation checks.
- [x] Preserve settings through the existing App config save seam; temporary host never overwrites the manual host. Correct the previous review's persistence statement.
- [x] Run full cloud_agent tests including real PTY/OAuth roundtrips; config/i18n gates. Add explicitly ignored real-network test using bundled helper and isolated empty test session, execute it separately if network permits. It does not launch Deppy GUI or certify a real Grokbot account.

### Task 4: Review, version, release build and record

Files: root `Cargo.toml`, `Cargo.lock`, AGENTS/handoff/report/plan, project journal.

- [x] Bump canonical workspace version to 0.2.0; regenerate lockfile through Cargo.
- [x] Actual CLI source review on changed production files; apply findings and rerun affected gates. Documentation is excluded from source review.
- [x] `cargo fmt --all -- --check`, `git diff --check`, `cargo test -p agent-mcp`, app cloud/config tests, packaging helper tests, boundary/dependency gates where affected.
- [x] Prepare helper, `cargo build -p deppy-sijo -p mcp-proxy --release`; verify compiled app-version metadata and staged signatures without modifying the running app's signed inode or launching another GUI.
- [x] Scoped logical commits after review, update continuous handoff and Obsidian project journal, report actual results and limitations. Do not push/restart without a separate current request.

## Executed results

- Helper preparation committed `4c9786a9`; application/server/config/UI/version committed `bb0441a9`.
- Helper4, MCP25, cloud21 (1 public test excluded by default), config45, i18n8 passed; public test explicitly executed separately and passed1 in80.79s on final source. Prior public run also passed80.95s.
- Source review found live tunnel health and old manual configuration migration issues; both reproduced with failing tests, fixed, and rereview reported no actionable remaining findings. Initial whole-suite fixture deadline race corrected and rerun passed.
- Release build, Developer ID signatures, bundle/archive contents and version0.2.0 verified. Package is a local development artifact without Apple notarization. No GUI restart, no push, no deployed Deppy gateway, no real Grokbot account certification.
- Durable evidence: `docs/reviews/measurements/2026-09-27-automatic-mcp-connect/results.json`; final report: `docs/reviews/2026-09-27-automatic-mcp-connect.md`.
