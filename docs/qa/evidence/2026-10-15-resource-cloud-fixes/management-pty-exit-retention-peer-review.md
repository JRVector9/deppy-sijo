# PTY exit-retention follow-up peer review — 2026-10-15

Source-review conclusion: no remaining actionable finding in the final shared-observer correction for the assigned invariants. This is static review, not behavioral GREEN. Root owns every Cargo execution and the final integration gates. No canonical or product-source files were edited by this reviewer.

## Exact reviewed revision

- Frozen worker tree: `/private/tmp/deppy-cloud-fix-pty-20261015`.
- Product patch: `/private/tmp/deppy-cloud-fix-pty-results-20261015/pty-waitid-owned-final-product.patch`, SHA256 `e2752bb96e8691564d61b0ddca907444e7780cc63d2c9dda054e1bbbeaa92818`.
- Combined archival patch: `pty-waitid-owned-final-combined.patch`, SHA256 `ded39e6e81906e261812d282e0ee61153249cbde4692203016f7745b5e6ade3b`.
- Cumulative original-baseline patch: `pty-waitid-owned-final-cumulative.patch`, manifest SHA256 `b0ad5d03695f227618499de6e33383a5f04e85cf24c4d16b0d6f7818d5b1dc4b`.
- Final `crates/pty/src/lib.rs`: `7f1f715f5dccc16570b0435e7f30bf7282ac9ea2748a7b847c3ecacdd1ab0e4b`.
- Final `crates/pty/src/workload_signal.rs`: `ac6172710590451cd7ff0008268f841faf0ba434e8836b2a740743a52541f0a4`.
- Final `crates/pty/src/foreground_teardown_tests.rs`: `0b33978efe953d0b5987df5544295490033487572ea070d8b593e3896fe46810`.
- Product delta is against canonical lib `edacafeabf3d94bb2fa88bbf718c6839b8c07f327b18e4b69099504c09a48e7c`; the final four natural-exit fixtures are already applied separately for Root's RED gate. Use the product patch for that integration state, not the cumulative replacement.

The product/combined patches and three source hashes above were independently recomputed with `shasum -a 256` and matched the final manifest. The cumulative hash is recorded from that manifest. Scoped `git diff --check -- crates/pty/src/lib.rs crates/pty/src/workload_signal.rs crates/pty/src/foreground_teardown_tests.rs` actually executed, exit0.

## Previous clean review invalidated by actual Darwin failure

My earlier clean source review of the inspect-zombie proposal (combined patch `9100e55986094cb385a0111e90d6b4055f56817c8203f0bd4696cfde5d736dfd`, followed by the one-line group-dedup revision) accepted an unqualified platform assumption: that `inspect` could return an exited, unreaped Darwin owner with its zombie bit. Root's actual gate disproved that assumption. That review must not be cited as successful validation of the old approach.

Root's preserved `/private/tmp/deppy-cloud-fix-root-20261015/pty-natural-exit-green.log.attempt1` records 64PASS/2FAIL/1ignored, exit101: the two new same-shell-group post-reap Drop/kill regressions failed. The six foreground/frozen cases passed. The failure is a product regression, not a reason to weaken the descendant assertion. Original runtime authorization handshake behavior remains required.

Root's actually executed native owned-child probe, `/private/tmp/deppy-cloud-fix-root-20261015/macos-zombie-probe.rs` and `.log`, establishes on this host:

- Live child: BSDInfo result136, valid identity/session; WNOWAIT returns pid0 when no exit is pending.
- Stopped child: WEXITED|WNOHANG|WNOWAIT can nevertheless return matching PID with CLD_STOPPED5. A matching nonterminal event must remain pending.
- Exited but unreaped child: proc_pidinfo returns0/ESRCH and getsid returns-1/ESRCH, while repeated WNOWAIT returns matching PID/CLD_EXITED1 without consuming the child.
- After the owned child is reaped, waitid returns ECHILD10.

Probe source SHA256 `1b251d426add8f98167146e2a01e08fe54a0a6f7907c14ec679df0b80786a246`; log SHA256 `98cd772e58bf7a4b32c33db9da2883513401a75b0e2e983fc9e767dbd6319816`; failed PTY gate log SHA256 `c99dafd1127724573eb9c4588896718fc89159890dfbed277f45bcc16dd042f7`.

Apple's [XNU waitid implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_exit.c) also documents that P_PID selects an owned child, WNOWAIT avoids reap, ECHILD means no matching child, and WNOHANG can leave siginfo untouched. The final code zero-initializes siginfo before each call. Host behavior above takes precedence over assumptions about the flag combination.

## Findings resolved during this review

The common observer now returns PendingOwned, ExitedOwned, Retired or Deferred from a fresh kernel observation. Successful WNOHANG with pid0 or a matching known nonterminal event proves the original direct child still exists without scanning groups or consuming status. Matching terminal events alone trigger the normal pre-reap snapshot. EINTR defers; ECHILD permanently retires authority; unknown or mismatching notifications fail closed.

The final capture entry uses that same observer under the child mutex. This closes both teardown-before-next-reaper-poll and live-peek→owner-disappearance gaps. If procinfo is unavailable, only a fresh owned-child observation plus original PID==PGID==SID permits capture, and only for the original shell group. portable-pty0.9's local Unix implementation unconditionally calls setsid before spawn returns. The unreaped original child therefore prevents those original numeric IDs from being reused while exact member identities are captured. There is no foreground fallback without a matching visible owner. A visible mismatching owner rejects further capture.

An explicit shared retired/reaped bit prevents a cached std Child PID from gaining fresh authority after consumption, including when the retained snapshot is empty or capture failed. Teardown retires authority before raw reap, including errors/timeouts, and skips direct kill if try_wait reports ECHILD. Retired/Deferred/error paths may use already retained fixed members but cannot add groups.

The fixtures now distinguish exited-unreaped from actually reaped. The periodic-reaper cases require ECHILD; inspect(None) is no longer accepted as proof of reap. Two additional cases stop/join a private concrete session's reaper before issuing the shell-exit command, then require matching non-consuming terminal status before Drop/kill. This deterministically exercises teardown before reaper polling. Same-group/live-descendant qualification, exact descendant identity, Python15s lifetime, and identity-checked rescue remain. No public API or runtime authorization fixture was changed.

## Invariants checked in the final source

- Lock order is child→exit-state everywhere. Normal reaper, try_exit_code, freeze and resume all call the common status helper while holding the child mutex. Capture holds that mutex through observation and every new member snapshot. No state→child route was found.
- Successful normal reap marks the shared state under the same child ownership lock. The raw Unix kill/reap helper retires it before all consuming paths. Spawn-failure cleanup owns the still-unpublished child exclusively; non-Unix branches retain their existing behavior.
- Regular 25ms polls perform one scalar non-consuming waitid, with no group scan on PendingOwned/Deferred. A terminal snapshot precedes actual reap. Retained fixed members survive later owner absence; teardown does not refresh members after retirement.
- FrozenGroup still rejects invalid/own process groups and foreign-session membership. Signal delivery rechecks each captured member's PID/birth/group/session, skips self/zombies/changed identities, and never substitutes an unpinned group signal after owner reap. The frozen rollback/foreign-group rejection logic is retained.
- Snapshot acquisition sends no signals. Existing guarded runtime authorization can finish before PTY destruction begins. Durable descendant TERM cleanup is restored through pinned members, while existing reader/writer cancellation, joins, 20ms HUP + 200ms TERM escalation and 2s bounded child reap remain.

## Remaining validation — Root only

No Cargo, build, app launch, native probe execution or behavioral test was executed by this reviewer. No final-fix test pass is claimed here. Root should finish the actual tests-only RED, final four samegroup descendant cases, all PTY tests, repeated unchanged runtime authorization handshake, full Runtime/workspace gates, and freshly linked real foreground/frozen workload harness. The static review does not replace those gates.


Root archive clarification: the earlier failed log was renamed to `.attempt1` before subsequent GREEN executions. Its source reference and digest above were updated to that preserved failure; final GREEN has a separate artifact. This changes only evidence location, not the reviewed product source.
