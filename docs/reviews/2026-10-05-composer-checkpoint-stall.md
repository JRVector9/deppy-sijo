# Composer draft checkpoint stall — 2026-10-05

User reported persistent “초안 저장 중…” and Enter not reaching the agent. This text came from the recent crash-recovery draft persistence feature. Routine autosave status was rendered above the input card even when no delivery was underway.

The writer preserved only the latest requested save status. A real controlled-worker test saved revision1 to disk while a later revision2 was deliberately blocked. The public status stayed Pending2, concealing the completed submission checkpoint from the App dispatch gate. RED reproduced this with an assertion failure; the separate routine-status test also failed. There was no deadline on the checkpoint wait; its expired-wait regression failed independently.

The writer now retains the highest successfully committed revision separately from its latest UI status. The App reads checkpoint_status for the required exact-or-newer marker. Existing failed/recovery conditions still fail closed and the original captured prompt/target/generation and single-dispatch rules remain. A pending save has a10s deadline, re-armed on intervening logic ticks. Expiration rejects only the known-unsent submission, retains its text, permits explicit retry, and cannot cause a late save to send it automatically. The checkpoint remains mandatory before PTY effects.

Routine autosave Pending text is hidden. Persistence error/recovery notices and retry stay inside the existing input card through a pure presentation hook. Known-unsent rejection also uses the inline delivery state without an external notification. No disk work was added to render, no extra worker/cache introduced, and runtime/wire/schema contracts did not change.

Executed focused verification: initial checkpoint3 and adjacent pr5_29 passed; final input-card geometry/retry1 and checkpoint3 passed. The actual egui test checks that editor and persistence error share the same stroked card, and clicking retry emits only the retry intent. Test-owned files only; no user prompt sent.

Investigation limits: the reported live Enter interaction was not reproduced against the user's agent. A read-only native process sample found the draft writer waiting on its Condvar, not blocked in filesystem I/O, and current persisted drafts had no uncertain marker. Do not claim a live filesystem hang. Running0.7.0 remains unchanged while fixes are built separately.

Version0.7.2→0.7.3;27 inherited workspace entries only. A version-edit assertion initially caught2 external dependencies sharing0.7.2 and stopped before changing lock. The subsequent locked test refused the temporarily stale lock. Corrected27 source-less workspace entries; dependency versions unchanged. The new UI fixture initially missed crate::config qualification; corrected before the successful run.

Final gates actually executed exit0:2792passed/0failed/34ignored across App+integration+i18n; strict App all-target Clippy-Dwarnings, UI boundary,27-crate dependencies and fmt/diff passed. Log /tmp/deppy-draft-stall-final-gates-20261005.log. Scoped manual self-review found no additional confirmed issue; no independent CLI review. Source commit/push and fresh signed artifact proof follow. Restart remains deferred by the user's instruction.


## Release record

- Product commit `22a344dacd3964cdee0cd65baa82358e9b7b49a2` pushed on `feat/audit-nine-pr-v0.6.0-20261004`.
- Gated offline/locked release build and package script exited0. Separately staged `target/restart-0.7.3-20261005/Deppy Sijo.app` and ZIP. Developer ID identity Vector Nine INC (ZDTU5LS35K); binary/bundle strict signature, architecture and archive verification passed under the explicit local development policy. No new notarization or public deployment.
- Both `CFBundleShortVersionString` and `CFBundleVersion` are0.7.3;27 inherited workspace lock versions match. Compiled native/About uses `CARGO_PKG_VERSION`, and the compiled binary contains0.7.3. The new native UI has not been launched or inspected.
- Proof `/tmp/deppy-rebuild-0.7.3-20261005-proof.json` verifies805 tracked product files unchanged between capture and post-build; source SHA256 `5c617f39025cfbb87522efcc2e0e4679415b90e76893496b769599601ba47a12`, binary SHA256 `34dfcbe3702ad9be117619a66922573ff41bd74bb4b3e13f4a29b26ec9acfd2c`. Build log `/tmp/deppy-rebuild-0.7.3-20261005.log`.
- Running0.7.0 PID26830 remains unchanged. No restart controller scheduled. Actual user-agent Enter interaction still needs verification after an authorized restart; the deterministic worker/gate and actual egui geometry regressions passed.
