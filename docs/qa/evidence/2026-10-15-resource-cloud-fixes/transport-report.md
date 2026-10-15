# R04 / R06 transport worker handoff — 2026-10-15

Current objective: Fix the qualified direct-WS whole authentication deadline and ended-target per-actor read-consent revocation defects, preserving normal fragments, stop, exact session identity and other approvals.

Completed work:
- Imported the existing qualified valid-auth slow-frame regression from `/private/tmp/deppy-cloud-review-root-20261015/direct-ws-tests-only.patch` before changing product code. Read the actual failing qualified log: auth deadline5s, elapsed6.363296458s, accepted=true,0passed/1failed.
- Imported the actual egui checkbox RED from `/private/tmp/deppy-cloud-review-logic-20261015/exited-read-revocation-tests-only.patch`. Read the actual failing log: retained read=true after click,0passed/1failed.
- Added internal AuthDeadlineStream below tungstenite. Each raw authentication read checks stop/deadline, applies timeout=min(remaining,300ms), and rechecks after the syscall. authenticate also rejects frames after the whole deadline/stop, and applies the same guard to raw writes/flushes, including automatic Pong/Close replies. Original socket read/write timeouts are snapshotted on entry and restored on successful authentication; failure retains its guard so error/close writes cannot escape timeout/stop.
- Kept serve and cloud-control public contracts unchanged; only private WebSocket transport types changed. The authentication wrapper temporarily owns a shared stop Arc, cleared on successful authentication and otherwise dropped with the owned socket after bounded failure replies. WebSocket and TcpStream retain sole owned socket lifetime, no helper threads or cloned sockets.
- Enabled read checkbox only for active actors with a live target or existing effective read grant. Existing ended grants can be revoked; ended targets without grants remain disabled. Input enablement stays live-only.
- Added normal masked fragmented-auth progress across a timeout with a real automatic Pong and original timeout restoration, stop during continuous valid partial-frame progress, deadline/stop interruption of queued real tungstenite Pong/Close flushes without buffer exhaustion, actor-consent preservation, disabled ended re-grant/input clicks, and other-session read/input preservation.

Modified files:
- crates/web-remote/src/ws_api.rs
- crates/app/src/cloud_agent/ui/permissions.rs
- crates/app/src/cloud_agent/ui/management_tests.rs

Key design decisions:
- Socket idle timeouts alone are insufficient because tungstenite may perform several successful raw reads inside one WebSocket::read call. Bound the raw read rather than restarting/copying tungstenite state; preserve partially received frames on WouldBlock/TimedOut.
- Stop before/during raw reads or writes/flushes returns ConnectionAborted. Expired whole deadline returns TimedOut, then authenticate's post-read whole-deadline check exits. Neither can admit a valid late Auth frame.
- Consent uses existing per-actor/per-exact-target controller revocation. No global actor revoke, other-session changes, new ended read grants or ended input grants were introduced.

Commands actually executed here:
- rustfmt --edition 2024 on the three owned files: exit0.
- git diff --check -- <the three owned files>: actual exit0, repeated after final source changes.
- git apply --check <scoped patch> against canonical: actual exit0 for the initial and final delivered patch; this is read-only, not integration.
- No Cargo, builds, application launches/restarts, real databases/accounts, commits or canonical/common-document writes were executed by this worker. No behavioral GREEN is claimed.

Failed approaches:
- Initial read-only wrapper scope was expanded after static review of tungstenite0.30 automatic reply flushes and Root instruction. One formatting-dependent follow-up patch application failed atomically, then was applied in smaller owned-file hunks. No implementation approach was behaviorally tested/rejected here. The only behavioural REDs used are the already executed review reproductions above. One exploratory rg referenced a nonexistent cloud_agent/permissions.rs; corrected to access.rs without changing source.

Remaining work:
- Root applies the scoped baseline-relative patch after checking each baseline hash and runs the actual test gates below. Root owns canonical integration, shared documentation/handoff, Cargo and complete test evidence.
- Existing lib.rs integration tests cover ordinary authenticated welcome/unauthorized close, socket-bound controls and WS slot restoration. No owned-file expansion was needed to duplicate those fixtures.
- Root later requests read-only peer review of metadata/cache implementation.

Exact Root commands after integration (Root alone runs Cargo; CARGO_BUILD_JOBS=2):
```sh
cargo test --offline --locked -p web-remote ws_api::tests:: -- --test-threads=1
cargo test --offline --locked -p web-remote ws_ -- --test-threads=1
cargo test --offline --locked -p web-remote -- --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent::ui::management_tests:: -- --test-threads=1
cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent::ui::redesign_tests:: -- --test-threads=1
```

New/ported behavioral tests:
- ws_api::tests::review_20261015_auth_rejects_valid_message_completed_after_whole_deadline
- ws_api::tests::auth_preserves_partial_frame_and_fragment_progress_across_read_ticks
- ws_api::tests::auth_stop_interrupts_a_continuously_progressing_valid_frame
- ws_api::tests::auth_deadline_and_stop_interrupt_automatic_pong_and_close_flushes
- cloud_agent::ui::management_tests::review_20261015_owner_can_revoke_read_consent_after_session_exit
- cloud_agent::ui::management_tests::management_ui_ended_read_revocation_preserves_other_session_consent

Publication/release state: source-only fix, no release/build delivered, no version change requested or performed by this worker. Application restart remains unauthorized.
