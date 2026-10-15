# CI fixture stability

Goal: Remove the two observed retry-dependent fixtures without weakening document conflict protection or HTTP authentication assertions.

Architecture: Document tests inject an external edit at the precise final-validation boundary through a test-only, thread-local, one-shot hook; document product behavior stays unchanged. HTTP investigation confirmed an actual response-loss defect, so the MCP server must half-close the response and perform bounded unread-body cleanup. Framed HTTP tests must reject truncation; changing the reader alone is insufficient.

Tech stack: Rust, existing app document I/O tests, agent-mcp loopback HTTP tests.

- [x] Inspect the actual first-attempt failures in the main integration CI log and both production paths.
- [x] Replace the scheduler-dependent observer in `crates/app/src/document_io.rs` with an isolated one-shot hook immediately before final revision validation. Keep assertions for Conflict, preserved external bytes, and temporary-file cleanup.
- [x] Execute `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo document_io::tests -- --test-threads=1` and a bounded repetition of the precise regression. Actual16PASS/3ignored, exact regression25/25 first-attempt PASS.
- [x] Verify MCP early-rejection framing with a private loopback probe; distinguish complete HTTP responses followed by reset from response truncation. Large early rejection lost headers in78/80 EOF samples and framed reads failed71/80. Valid large control40/40 complete. This is a product teardown defect, not just a test EOF assumption.
- [x] Add bounded framed-response controls and actual early/busy rejection regressions in `crates/agent-mcp/src/tests.rs`. Actual root RED0PASS2FAIL exit101; no reset was ignored.
- [x] Fix `crates/agent-mcp/src/server.rs` response teardown with a write half-close, absolute cleanup deadline and byte cap. Preserve ordinary call capacity and normal completed-body latency; do not wait for an arbitrary declared body or EOF.
- [x] Execute `cargo test --offline --locked -p agent-mcp -- --test-threads=1` and repeat the affected actual HTTP test with no retry-based acceptance.
- [x] Review the diff, run affected formatting/Clippy checks, record exact commands and outcomes in `docs/CODEX_HANDOFF.md`, and commit this repair independently from the IME fix and cloud feature.

Execution is orchestrated by root. Cargo remains single-owner with two build jobs; workers must not run against the shared target concurrently. Do not launch the app, change user files, or send real provider messages.

Actual final results: MCP74PASS/0FAIL/1ignored; fresh production loopback400/400 complete responses with0 resets/errors/missing/truncated responses. Full workspace5743PASS/0FAIL/65ignored before the final cap-test-only commit; Clippy/fmt/boundary/deps all exit0 after that commit. No provider messages or app launch. Root product commits fdc27f6b(document fixture), 6796930c(MCP RED), f5e58d5f(MCP fix), a4631474(cap regression). Raw logs and cohort metadata are under `docs/qa/evidence/2026-10-15-ime-cloud-followup/`.
