# Composer draft checkpoint stall — 2026-10-05

User reported persistent “초안 저장 중…” and Enter not reaching the agent. This text came from the recent crash-recovery draft persistence feature. Routine autosave status was rendered above the input card even when no delivery was underway.

The writer preserved only the latest requested save status. A real controlled-worker test saved revision1 to disk while a later revision2 was deliberately blocked. The public status stayed Pending2, concealing the completed submission checkpoint from the App dispatch gate. RED reproduced this with an assertion failure; the separate routine-status test also failed. There was no deadline on the checkpoint wait; its expired-wait regression failed independently.

The writer now retains the highest successfully committed revision separately from its latest UI status. The App reads checkpoint_status for the required exact-or-newer marker. Existing failed/recovery conditions still fail closed and the original captured prompt/target/generation and single-dispatch rules remain. A pending save has a10s deadline, re-armed on intervening logic ticks. Expiration rejects only the known-unsent submission, retains its text, permits explicit retry, and cannot cause a late save to send it automatically. The checkpoint remains mandatory before PTY effects.

Routine autosave Pending text is hidden. Persistence error/recovery notices and retry stay inside the existing input card through a pure presentation hook. Known-unsent rejection also uses the inline delivery state without an external notification. No disk work was added to render, no extra worker/cache introduced, and runtime/wire/schema contracts did not change.

Executed focused verification: initial checkpoint3 and adjacent pr5_29 passed; final input-card geometry/retry1 and checkpoint3 passed. The actual egui test checks that editor and persistence error share the same stroked card, and clicking retry emits only the retry intent. Test-owned files only; no user prompt sent.

Investigation limits: the reported live Enter interaction was not reproduced against the user's agent. A read-only native process sample found the draft writer waiting on its Condvar, not blocked in filesystem I/O, and current persisted drafts had no uncertain marker. Do not claim a live filesystem hang. Running0.7.0 remains unchanged while fixes are built separately.

Version0.7.2→0.7.3;27 inherited workspace entries only. A version-edit assertion initially caught2 external dependencies sharing0.7.2 and stopped before changing lock. The subsequent locked test refused the temporarily stale lock. Corrected27 source-less workspace entries; dependency versions unchanged. The new UI fixture initially missed crate::config qualification; corrected before the successful run.

Final gates actually executed exit0:2792passed/0failed/34ignored across App+integration+i18n; strict App all-target Clippy-Dwarnings, UI boundary,27-crate dependencies and fmt/diff passed. Log /tmp/deppy-draft-stall-final-gates-20261005.log. Scoped manual self-review found no additional confirmed issue; no independent CLI review. Source commit/push and fresh signed artifact proof follow. Restart remains deferred by the user's instruction.
