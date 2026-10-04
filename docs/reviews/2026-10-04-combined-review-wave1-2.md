# Independent combined source review — five findings corrected

Reviewer: new explicitly configured gpt-6.1-sol/xhigh agent. Immutable actual source38ee3bece2d0369eddd763d79cd1cbcfe2f2e5b6 compared against original complete source85631a845d733b340e281df976522be4d4468959. Source only; no tests/builds/nativeapp actions by reviewer.

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| medium | storage/src/logs.rs:164 | Cached EOF + external append + partial failure | Can exceed file cap after error | Real fixture RED, bounded error recovery |
| medium | app/src/ui/composer.rs:751 | Rejected/unknown payload retention outside budget | Historical delivery Arc accumulation | Include in PR5 budget/permanent lifecycle |
| medium | app/src/app.rs:16568 | Closed follow-up reservations lack aggregate cap | Inaccessible payload accumulation | Bounded retained reservations/lifecycle |
| medium | app/src/ui/fleet.rs:1003 | Follow-up editor/insertion/per-frame clone unbounded | Memory growth and rejected action loses form | Shared bounded edit/insertion/borrow |
| medium | app/src/ui/fleet.rs:1337 | Batch UI1MiB vs host16KiB | Form closes before host rejects | Reject before enabling Start |

Concrete log source condition: cap64, own append32, external handle append32, own append20 partially writes5 and fails before successful-write cap reconciliation; length69. Existing separate external/partial tests did not combine those conditions. No test execution yet for this finding.

No additional reliable findings in atomicbody/submit, ACKcorrelation, execution/foreground/localdraft/dialog, startup/recovery/checkedserialsave, Markdowngeneration/samehandlevalidation, or async historyclaim/revalidation/finish. This is scoped source-review evidence, not full application correctness proof. The table records original findings; completed corrective evidence follows.

## Correction results

- Log cap: `3cfe880c88e0a781a3d173458792d9361c518876`; combined external append/partial failure RED reproduced 69 bytes at cap64, bounded error recovery GREEN and full Storage408 passed.
- Composer retention: PR5 `78371a7f0b51531160810ae5bc71dc93ee277f81`; separate delivery/live draft budgets, stable session identity, durable Pending warning and shutdown/delete policy. PR5 29 and Composer72 passed; cleared String capacity 9,830,400→0B measured.
- Follow-up retention/editor/batch limit: `809bda2f8842e2840f2d4d65f2adc29354801219`; aggregate reservation/metadata budgets, exact settlement, accessible blocked reservation cancellation, bounded shared editor/undo, Batch UI16KiB. PR4 26 and Fleet43 passed; independent frozen-source review confirmed no remaining finding in that scope.
- Full coherent nine-PR suite subsequently passed 4,754 tests, 47 existing ignored, strict workspace Clippy and fmt. This is the pre-final-corrective full gate, not the final release source gate.
- The next full nine-PR CLI source review found three additional issues. PR13 queued-output freshness/fast-exit final frame, PR14 pending reservation authorization revoke, and PR15 generated ID/New-form recovery have been corrected and integrated. Root named corrective gate passed runtime7/PTY1/App10+2 and strict workspace all-target Clippy/fmt; subsequent corrective CLI found the PTY destructor under authorization locks; PR13r corrected it with real private termination RED/GREEN, and the final tiny source review confirmed no remaining finding in that scope. Final integrated gate after all follow-ups:4,826passed/0failed/47existingignored, strict workspace gates exit0; see the final improvements report.

## PR9 독립 소스 리뷰

- reviewer `combined_review`, gpt-6.1-sol/xhigh, immutable38ee3bec→e2cd2c8 crates diff와 관련 admission/actor 검토: 확인된 도입 버그 없음. 테스트는 이 리뷰에서 실행하지 않음.
- 초기 grant 제거/replacement revocation 지적은 실제 baseline/commit 모두 `impl Drop for Grant { self.permit.revoke(); }`가 있음을 root와 reviewer가 재확인하여 철회. 불필요한 코드 변경/회귀 테스트를 추가하지 않음.
- 9r 준비 snapshot0f7d45dd와 worktree는 미사용 보존; 적용하지 않음.
