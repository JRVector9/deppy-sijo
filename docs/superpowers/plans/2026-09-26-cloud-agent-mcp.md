# Cloud Agent MCP Implementation Plan

> **For agentic workers:** Execute this plan inline, task by task. The user has authorized implementation. Do not launch or restart Deppy.

**Goal:** Grok Bot and compatible cloud agents can read and control explicitly shared Deppy sessions and send their own answers back to the original session.

**Architecture:** A dedicated loopback MCP Streamable HTTP endpoint uses expiring bearer tokens and a bounded mailbox to the App thread. App validates stable session UUID and concrete runtime generation immediately before dispatch. Existing runtime input queues and notification UI remain the effect boundaries; local SQLite records answers and input audit entries. No custom relay, offline delivery queue, pairing code, or webhook receiver is included.

**Tech Stack:** Rust, synchronous TCP, existing web-remote HTTP parser, serde JSON, SQLite, egui.

---

## PR 1 — MCP transport, authentication, session reads

Files: `crates/agent-mcp/{Cargo.toml,src/lib.rs,src/server.rs,src/tests.rs}`, workspace Cargo manifests, `crates/web-remote/src/http.rs`.

- [x] Add failing tests for unauthenticated requests, Origin validation, token expiry/revoke, initialize/tool discovery, and bounded transport.
- [x] Implement `/mcp` POST JSON responses; GET returns 405 (SSE is optional). Support MCP 2025-03-26, 2025-06-18, 2025-11-25 negotiation. Bind 127.0.0.1 only.
- [x] Define `list_sessions`, `read_output`, `send_text`, `send_ctrl_c`, `notify`. Each tool accepts explicit session identity; mutation requires generation and operation ID.
- [x] Verify `cargo test -p agent-mcp` and review source diff with `codex review`.
- [x] Commit `feat: 클라우드 에이전트 MCP 전송과 인증을 추가한다`.

## PR 2 — App routing, input permissions, durable history

Files: `crates/app/src/cloud_agent.rs`, `crates/app/src/app.rs`, `crates/app/src/main.rs`, `crates/app/src/ui/workspace.rs`, agent-mcp history module.

- [x] Test generation mismatch, session closure, queued request after revoke, newline/control rejection, and duplicate operation IDs before implementation.
- [x] Route directly to original active/warm runtime without switching tabs. Shared sessions default off; input separately defaults off and resets on restart.
- [x] Reject CR/LF and control characters in ordinary text. `submit=true` is the only way to append Enter. Ctrl+C is a separate tool. ACK explicitly describes runtime queue acceptance, not command completion; uncertain submissions are never automatically retried.
- [x] Persist an operation claim before dispatch. Repeated operation IDs return the recorded outcome; crash ambiguity remains unknown. Persist bounded input audit metadata, with no raw terminal commands or secrets in application logs.
- [x] Store answers (up to 16 KiB) in a dedicated local SQLite database; acknowledge only after commit. Read-only shared sessions may receive answers. Removing input permission preserves received answers.
- [x] Verify focused tests and commit `feat: 세션별 클라우드 입력과 답변 기록을 연결한다`.

## PR 3 — Connection UI, take control, Grok answers

Files: `crates/app/src/ui/settings.rs`, `crates/app/src/cloud_agent.rs`, `crates/app/src/ui/notifications.rs`, locale message catalogs.

- [x] Add Settings > Cloud agents with Start/Stop, masked token reveal/copy, expiry, rotate/revoke, and HTTPS endpoint instructions.
- [x] Show sessions across workspaces with separate Share/read and Allow input toggles. Take control revokes all remote input immediately and retains read access and answers.
- [x] `notify(session_id, generation, operation_id, message)` writes the Grok/cloud agent answer to history and posts an existing Deppy notification pointing to the original session. Never write answers to PTY stdin.
- [x] Show full persisted answers and input audit history in the same settings area, including original workspace/session and timestamp. Allow copying answer text.
- [x] Verify UI state/model tests and commit `feat: 클라우드 연결 설정과 그록 답변 알림을 추가한다`.

## PR 4 — Setup guide, end-to-end verification, final review/build

Files: `docs/cloud-agent-mcp.md`, `docs/CODEX_HANDOFF.md`, agent-mcp integration tests.

- [x] Document HTTPS via named Cloudflare Tunnel or Tailscale Funnel on a dedicated port, connector bearer auth, and a Grok instruction requiring `notify` after each answer.
- [x] Verify real HTTP initialize → discovery → list/read → forbidden input → authorized input → notify → duplicate retry → revoke, with a local fixture. This does not certify the user's Grok Bot connector.
- [x] Run final `codex review` on source, address findings, focused tests, `cargo build -p deppy-sijo --release`, and `git diff --check`.
- [x] Record actual results, build path and remaining external account/tunnel validation. Commit documentation/review corrections. App remains running as it was.

## Contract and acceptance

- Dedicated port 8739; token valid 24 hours, generated in memory and cleared on stop/revoke/restart.
- `list_sessions`: only explicitly shared sessions; UUID, workspace, title, generation and input permission.
- `read_output`: cursor-based latest visible-screen updates; reports reset when cursor/session incarnation is stale. This is a screen snapshot feed, not a lossless stdout stream.
- `send_text`: session_id + generation + operation_id + text + submit (default false). Maximum 8 KiB, no CR/LF/control escapes. The response cannot claim shell completion.
- `send_ctrl_c`: same identity/idempotency inputs; emits one byte 0x03 only while input is allowed.
- `notify`: same identity/idempotency inputs + message. Maximum 16 KiB; answer remains in history after disconnect/restart.
- Expired/revoked mailbox requests must be rejected before side effects. Auth generation is rechecked at App dispatch.
- SQLite retains last 500 records; answer and audit history survives token rotation. Raw input is not persisted; audit records byte count/submit/outcome.
- Public URL/account configuration and actual Grok registration require user-specific endpoint details. No public tunnel is started automatically.

## Sources

- [MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [Grok connectors](https://docs.x.ai/grok/connectors)
- [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/)
- [Tailscale Funnel](https://tailscale.com/docs/features/tailscale-funnel)

## Completion — 2026-09-26

- PR 1: `e872e861`; PR 2: `ab49b177`; PR 3: `ee58a2f2`; PR 4: final review fix/setup-guide commit follows this record. These are implementation units on `feat/cloud-agent-mcp`; GitHub PRs have not been published.
- 93 focused tests passed: agent-mcp 9, App bridge 7, notifications 25, config 43, i18n 8, notification navigation 1.
- CLI reviews: 2 P1 and 6 P2 findings fixed. The final targeted navigation review reported no remaining actionable defect.
- `check-boundary`, `check-deps`, `cargo fmt --all -- --check`, and `git diff --check` passed.
- Final `cargo build -p deppy-sijo --release` passed. Executable: `target/release/deppy-sijo`. App was not launched/restarted.
- The actual user's public tunnel/Grok Bot connector has not been configured or verified; local HTTP fixtures do not certify external provider compatibility.
