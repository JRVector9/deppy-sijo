# PR2 — AI prompt eligibility and input draft protection

## Objective and completion

Baseline `ed2c9163b21516b704234d5c20360dfa80d2d2bc`, isolated worktree `/private/tmp/deppy-audit-pr2-wave2-coherent-20261004`, branch `fix/audit-pr2-ai-eligibility-wave2-coherent-20261004`. Implemented frozen AI execution targeting and runtime admission checks for Fleet broadcast/followup, Composer, and deliberate selected-text paste. Ordinary shell typing remains available. No native Deppy or real AI launch, user terminal/clipboard actions, version/Cargo.lock changes, push, or shared handoff edits were performed. Parent owns integration, final CLI review, full suites, version and build.

## Modified files

- `crates/app/src/agent_detect.rs`, `agent_detect_worker.rs`, `proc_info.rs`: process identity capture and validation, bounded Linux birth parsing, fixtures.
- `crates/app/src/fleet.rs`, `ui/fleet.rs`: immutable eligible selection/followup targets and stale-target tests.
- `crates/app/src/app.rs`, `ui/workspace.rs`: captured Composer context, shared guarded sender, bounded tracked no-submit selection paste.
- `crates/runtime/src/input_admission.rs`, `in_process.rs`, `lib.rs`: reusable input intent guard and admission-time validation.
- `crates/session/src/status.rs`, `session.rs`: constant-space accepted-input draft evidence and nonmutating viewport/foreground access.
- `crates/pty/src/lib.rs`: actual Unix terminal foreground process-group query.
- This scoped implementation/review/test report.

## Design and reusable API

`AgentExecutionIdentity` freezes provider kind, owner PID, kernel birth and process group. Actual detector rows capture it; cached bindings require birth equal to the recorded cache birth. `is_current()` rechecks the kernel birth and process group. Display names, stale model labels and an open PTY are not eligibility evidence.

`FleetPromptTarget` freezes workspace ID, runtime instance, session and execution identity. Ordinary shells, structured sessions and missing identities cannot be selected for automatic prompts. Replacement AI or runtime creates a different selection key. Followups retain the original captured target and payload rather than silently rebinding to a new AI or fallback shell.

The local-only runtime contract is:

```rust
AgentInputIntent::{AutomaticPrompt, ExplicitPrompt, ExplicitAppend}
AgentExecutionIdentity::is_current()
AgentExecutionIdentity::input_guard_for(intent) -> AgentInputGuard
InputAdmission::with_agent_guard(guard) -> InputAdmission
```

`input_admission(automatic, deadline)` / `input_admission_for(intent, deadline)` are local convenience methods. A cloud caller must keep its existing grant permit, authentication check and deadline, combine `is_current()` with that authorizer, and attach `input_guard_for(intent)` to the same `InputAdmission`. Replacing the cloud grant with a fresh local permit would break revocation; PR9 must not do that. These fields are not serialized; PR1 runtime wire version 22 is unchanged by PR2.

Guard checks run inside the authorization callback immediately before the existing one-reservation atomic PTY batch. It compares actual foreground process group, current accepted local draft evidence, detector approval/dialog status, and a bounded nonmutating cursor-row/nearby-choice viewport snapshot. It does not consume viewport dirty ranges or publish hidden panes. PR1 operation receipts continue to mean admission, not AI execution/completion; rejected Composer drafts and blocked followup reservations are retained.

Supported Claude/Codex cursor rows distinguish empty prompts, existing text and dim suggestions. Automatic input requires a verified empty prompt, so unknown readiness (including Other/Grok/Kimi) fails closed. Explicit visible Composer remains useful for unsupported readiness parsers: it checks exact identity, foreground, known local draft and dialogs without inventing provider readiness. `ExplicitAppend` permits intentional existing draft text but still checks identity/foreground/dialog safety.

Selected-text paste now captures `(session, execution)` in the menu and queues bounded no-submit intent (32 queued items / 1 MiB total) for the common sender. The UI leaf performs no process IO. Existing provider paste encoding is reused. With DEC2004 off and non-Codex provider, CR/LF become spaces so a manual append cannot accidentally submit. With Codex/bracketed mode, the existing provider-specific plan is retained. This preserves `explain this:` plus selected text workflows; no Enter is appended.

Draft evidence uses a constant-space bounded escape parser. Left/Right/Home/End, empty backspace, focus sequences and empty split bracketed paste do not create phantom text. They do not erase genuine draft evidence. Up/Down (CSI/SS3) and Ctrl-P/N conservatively mark history-recall uncertainty before output arrives. Paste-body CR/LF remains draft; actual unbracketed submit/Ctrl-C boundaries clear evidence. Redraw and turn hooks cannot erase it.

## Observed RED and failed approaches

- Actual Fleet test `pr2_ordinary_and_exited_shells_are_not_ai_broadcast_targets`: old `broadcast_key` selected an ordinary shell, assertion `Some((local, SessionId(7))) != None`, 0 passed / 1 failed. Source-clean gate log `/tmp/deppy-pr2-red-20261004.log`. Initial fixture integer Mux IDs failed compilation and were corrected before this RED.
- Actual runtime test `pr2_actual_admission_preserves_existing_tui_draft_and_dialog`: accepted `draft` followed by guarded body+CR was incorrectly admitted, 0 passed / 1 failed. Log `/tmp/deppy-pr2-draft-red-20261004.log`. Initial nonexistent `StatusPatterns::default` fixture API failed compilation and was replaced with the actual compile API before this RED. After draft protection, its dialog assertion reproduced accepted input at an unclassified choice dialog; bounded visible choice evidence fixed it.
- `pr2_navigation_and_empty_split_paste_do_not_create_a_phantom_draft`: empty split Left/focus sequence set a sticky draft, 0 passed / 1 failed. Log `/tmp/deppy-pr2-navigation-red-20261004.log`. Replaced byte-level dirtiness with the bounded escape parser, then added conservative Up/Down-history coverage at actual runtime admission.
- Broad HashSet replacement initially affected attention clocks; narrowed the change to broadcast targets. A fixture shell quoting parse error was corrected to a raw Rust string. App could not call the runtime's cfg(test)-only constructor; the integration fixture uses public `try_new_with_resolver` and private `/bin/sh` SpawnAgent instead. Import/tempdir fixture mistakes were corrected before final proof. Own strict Clippy needless/nonminimal bool and item placement findings were fixed rather than suppressed.
- Earlier ungated/stale-source test counts are not final evidence. The final gate cleans workspace artifacts on worktree source switches and holds its lock through compilation and test execution.

## Final executed verification

The following consolidated command completed with exit 0 after source cleanup. Log: `/tmp/deppy-pr2-final-batch-v5-20261004.log`.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["test","--offline","--locked","-q","-p","session","pr2_","--","--list"],
 ["test","--offline","--locked","-q","-p","runtime","pr2_","--","--list"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr2_","--","--list"],
 ["test","--offline","--locked","-q","-p","session","status::","--","--test-threads=1"],
 ["test","--offline","--locked","-q","-p","runtime","pr2_","--","--nocapture"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr2_","--","--nocapture"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","fleet::","--","--test-threads=1"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","agent_detect::tests::","--","--test-threads=1"],
 ["clippy","--offline","--locked","-q","-p","deppy-sijo","--tests","--","-D","warnings"],
 ["clippy","--offline","--locked","-q","-p","runtime","-p","session","-p","pty","--all-targets","--","-D","warnings"]
]' > /tmp/deppy-pr2-final-batch-v5-20261004.log 2>&1
```

| Check | Actual result |
| --- | --- |
| Named PR2 list | 2 Status / 3 Runtime / 7 App tests present |
| Session status tests | 36 passed, 0 failed, 0 ignored |
| Runtime PR2 tests | 3 passed, 0 failed |
| App PR2 tests | 7 passed, 0 failed |
| Fleet tests | 51 passed, 0 failed, 1 preexisting ignored |
| Agent detector tests | 61 passed, 0 failed, 1 preexisting ignored |
| App strict test Clippy | Exit 0 |
| Runtime/session/pty strict all-target Clippy | Exit 0 |

Final `cargo fmt --all -- --check` and `git diff --check` were executed after report completion and both exited 0. `git diff -- Cargo.toml Cargo.lock` produced no changes.

The actual fallback fixture freezes a real private child PID/birth/group, admits a positive control, terminates only that recorded fixture child, then confirms the still-live terminal's `/bin/cat` fallback rejects the entire natural prompt+CR. The actual rejection retains the Composer draft; ordinary manual-shell input is admitted. No real AI provider is launched.

An earlier full App all-target Clippy run (v4 log) exposed unchanged baseline PR3 production-unused PromptLibrary wrappers. Parent confirms integrated PR4 `314ecf37` marks those wrappers cfg(test), resolving it in the integration tree. PR2 leaves PR4-owned code unchanged; parent runs the final integrated full gate. No full App production-Clippy pass is claimed for this isolated baseline.

## Platform and integration limits / remaining work

macOS kernel identity and private Unix PTY foreground/fallback checks were executed. Linux now uses a bounded 4096-byte `/proc/PID/stat` birth read (O_NOFOLLOW/O_NONBLOCK, recorded PID validation and checked field-22 start ticks); parser fixtures cover comm spaces/parentheses, wrong PID, overflow and oversize on macOS. No Linux native build or runtime is claimed. Unsupported OS/Windows exact AI guard currently fails closed while manual ordinary-shell input remains available; no fake PID-only identity is used.

Focused source review covered callers, target retention, actual authorization timing, no-submit encoding, local draft/dialog boundaries and platform capture. Native UI/provider smoke tests and RSS measurements were not run. No additional confirmed PR2 defect remains from that review.

Parent integration actions:

1. Cherry-pick this scoped commit and preserve PR5 Composer-context changes during its later merge.
2. Apply PR4 bounded Fleet parameter/render/preview helpers in root glue; those existing Fleet parsing/render blocks were intentionally left unchanged in PR2.
3. PR9 new explicit paste uses captured identity and caller-supplied grant/auth admission. Legacy `send_text` retains its existing authorized raw typing contract, including shells/dialogs; do not apply the new whole-prompt policy to it.
4. Run coherent integrated full suites/CLI review, then root-owned release version/build/artifact verification. Do not restart Deppy.
