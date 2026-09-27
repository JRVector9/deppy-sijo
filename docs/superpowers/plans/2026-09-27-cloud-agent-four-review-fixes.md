# Cloud agent four review fixes Implementation Plan

> Execute inline in the current authorized task. Preserve other worktrees and do not launch/restart Deppy.

**Goal:** Fix all four confirmed defects in `docs/reviews/2026-09-27-cloud-agent-revocation-review.md`.

**Architecture:** OAuth validates final encoded redirect bounds before consent and treats scopes as sets. Cloud input carries a revocable local admission permit alongside the existing runtime command; the permit lock covers only the PTY queue write, so revocation synchronizes with admission without holding it across UI wake callbacks. Final no-effect outcomes complete durable claims.

**Tech Stack:** Rust, std sync, SQLite, existing local HTTP/MCP/runtime and egui.

## Task 1 — OAuth redirects

Files: `crates/agent-mcp/src/oauth.rs`, `crates/agent-mcp/src/tests.rs`, `crates/web-remote/src/http.rs`.

- [x] Add a regression with a registered2048-byte HTTPS redirect and2048 unreserved tildes in state. Assert authorize rejects before local approval and produces a valid bounded error response; exercise accepted long-state success/denial redirects through the shared writer.
- [x] Run `cargo test -p agent-mcp redirect_bounds -- --nocapture`; observe RED before product edits.
- [x] Use a shared response-header limit. Build redirect with a helper and validate worst-case fixed64-byte code plus encoded state at authorize time. Build the completed redirect before removing pending consent.

```rust
fn redirect_location(redirect: &str, state: &str, code: Option<&str>) -> String {
    let mut url = url::Url::parse(redirect).expect("validated redirect");
    let mut query = url.query_pairs_mut();
    query.append_pair(if code.is_some() { "code" } else { "error" }, code.unwrap_or("access_denied"));
    query.append_pair("state", state);
    drop(query);
    url.into()
}
```

- [x] Run the regression and real HTTP OAuth tests to GREEN.

## Task 2 — Refresh scopes

File: `crates/agent-mcp/src/oauth.rs`.

- [x] Add authorization→PKCE exchange→refresh regressions for reversed scope order, read-only subset, invalid expansion and unknown scope. Verify the reduced access token cannot type and its rotated refresh grant retains original scope.
- [x] Run `cargo test -p agent-mcp refresh_scope -- --nocapture`; observe RED.
- [x] Parse the requested scope tokens, require read, reject unknown/expanded input, issue requested access scope, retain original refresh scope during rotation.

```rust
let access_input = match q.get("scope") {
    None => r.input,
    Some(scope) => {
        let tokens: Vec<_> = scope.split_whitespace().collect();
        if !tokens.contains(&"deppy.read") || tokens.iter().any(|s| !matches!(*s, "deppy.read" | "deppy.input")) || (!r.input && tokens.contains(&"deppy.input")) {
            return error(400, "invalid_scope");
        }
        tokens.contains(&"deppy.input")
    }
};
```

- [x] Run `cargo test -p agent-mcp` and source-only CLI review; resolve findings. Commit the OAuth unit with a Korean fix message.

## Task 3 — Durable no-effect receipts

Files: `crates/app/src/cloud_agent.rs` tests/execute.

- [x] Add the actual SQLite exclusive-lock regression: lock80ms, request25ms, assert no send; same operation ID retry must return a rejected/no-effect receipt instead of unknown.
- [x] Run `cargo test -p deppy-sijo --bin deppy-sijo claim_expiry -- --nocapture`; observe RED.
- [x] Finalize the claim before returning the no-effect result; preserve tombstone and conservative unknown on database finish failure.

```rust
let outcome = json!({"status":"rejected","error":"expired_or_revoked_request_no_effect","retry":false});
db.finish(op, &outcome, "").map_err(|_| "outcome_unknown_do_not_retry_input")?;
return Ok(outcome);
```

- [x] Run the focused regression to GREEN.

## Task 4 — Revocation at PTY admission

Files: create `crates/runtime/src/input_admission.rs`; modify runtime lib/in_process/protocol, `crates/pty/src/input_queue.rs`, App cloud_agent/app.

- [x] Add the real runtime gate regression from `/tmp/deppy-cloud-revoke-probe/main.rs`: pause worker, queue cloud input, take_control, resume; assert `InputAdmitted` rejection and no terminal marker. Test unshare, token rotate, stop, generation change and expired admission with the same gate where applicable.
- [x] Run the regression on the current implementation to observe RED.
- [x] Add `InputPermit` backed by `Arc<Mutex<bool>>` and `InputAdmission` with deadline and a live-auth callback. `revoke` and PTY queue write share the permit lock. Keep pressure events/detectors/wake outside that lock.

```rust
pub fn send_guarded_input(&self, session: SessionId, operation_id: String, bytes: Vec<u8>, admission: InputAdmission) -> anyhow::Result<()>;
```

- [x] Carry optional admission only in the local `QueuedRuntimeCommand` envelope; keep existing command bytes unchanged. Route cloud input through the guarded API. Revoke permits on disable/unshare/target removal/stop/rotate and replace them when re-enabled.
- [x] Append `AdmissionDenied` to the PTY reject reason; update protocol to19 because a new wire error value must be rejected by older peers. Runtime has no MCP dependency.
- [x] Run App cloud_agent, runtime guarded admission/tracked/command/protocol/old-peer tests, boundary/dependency checks and formatting. Source-only CLI review and fix concrete findings.
- [x] Commit the admission/receipt unit, update setup guide/review status/handoffs/project journal, run `cargo build -p deppy-sijo --release`; never launch it.

## Completion evidence

Record every RED/GREEN command and actual result in `docs/CODEX_HANDOFF.md`. Actual external Grok Bot/public HTTPS remains unconfigured and is not a code defect completed by this task.

## Final completion — 2026-09-27

Source commits `36e987c9`, `37c808c1`. MCP24/App12 and runtime tracked12/command27/protocol2/old-peer1 passed; boundary/dependency/format checks passed. Final current atomic CLI review has no actionable remaining defect. Release rebuild succeeded24.46s. Report: `docs/reviews/2026-09-27-cloud-agent-four-fixes.md`. User GUI not launched/restarted.
