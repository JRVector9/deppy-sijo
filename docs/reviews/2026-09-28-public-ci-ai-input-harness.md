# Public CI and AI terminal input harness — 2026-09-28

## CI root cause and public rerun

PR #202 was initially blocked before a runner started. GitHub check-run
108732729009 recorded: “The job was not started because recent account payments
have failed or your spending limit needs to be increased.” The original job had
no runner, no steps, and 0 billable milliseconds. This was an account billing
gate, not a failing test assertion.

The user authorized public visibility. A pre-publication review found no
tracked secret-like files in current or historic path names and no current-tree
key-pattern matches beyond test/example placeholders. The repository was
changed to PUBLIC and both failed workflows were rerun. Actual GitHub runners
then started. Relay WebCrypto, Linux relay, formatting/boundary, and
license/source checks passed. macOS Clippy exposed a type-complexity warning;
RustSec found rustls 0.23.43 advisory RUSTSEC-2026-0285. Local Clippy was run
through the full workspace and all newly surfaced warnings were fixed.
`rustls` was updated to 0.23.45 with `rustls-webpki` 0.103.15. Local
`cargo audit --json` now reports 0 vulnerabilities and 2 yanked-package
warnings.

## AI terminal input investigation

Primary references: [Warp Korean IME Enter report](https://github.com/warpdotdev/warp/issues/8919),
[cmux broad IME interception regression](https://github.com/manaflow-ai/cmux/issues/3762),
[cmux narrower fix](https://github.com/manaflow-ai/cmux/pull/3867), and
[cmux modifier-state report](https://github.com/manaflow-ai/cmux/issues/2949).
These reports support preserving the actual composition/Commit boundary and
avoiding interception based only on the selected input source.

The real workspace UI frame harness at
`crates/app/src/ui/workspace/tests/ime_input_harness.rs` reproduced dropped
last syllables when an empty preedit preceded a pane switch or an absent Commit
timeout. It also reproduced late Commit routing to the new session, including
punctuation and a paired Text echo. The fix keeps the pending submission owned
by the original session for a bounded period, completes it when the Commit
arrives, and routes the resulting PTY bytes before Enter. A runtime focus
acknowledgement leaves that pending input intact. Physical key observations
before and after the old Enter distinguish old Commit punctuation from a new
pane's independent punctuation. TextEdit-owned IME batches remain with TextEdit.
The harness checks session IDs and exact PTY bytes, including no duplicate
delivery to the new session. Source review exposed four additional ownership
cases: timeout while TextEdit owns the keyboard, a clipboard result after
detachment, a paired Key event, and a paired Text event after timeout. Each was
reproduced as a failing harness trace before the implementation was corrected.
A later source review found three more: full Text echo mixed with a new physical
key, a Key inserted between Commit and its Text echo, and an independent
clipboard period incorrectly deduplicated against Commit punctuation. All
three were reproduced as failing traces and then corrected. Clipboard bytes
requested before Enter now have separate provenance from physical punctuation.
The final review found that a bare suffix Text is ambiguous on platforms
without AppKit's native key observations. The echo filter now retains such
input there, while still consuming a full Commit echo or a Key+Text pair.

## Verification and limits

- Focused input harness after latest fixes: 26 passed, 0 failed.
- Full `cargo test --workspace --locked -- --test-threads=1` on final source:
  69 suites, 4,449 passed, 0 failed, 36 ignored. Public CI will run after push.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- `cargo audit --json`: 0 vulnerabilities; 2 yanked-package warnings.
- Version 0.2.3 → 0.2.4; separate local bundle path
  `target/bundle-0.2.4/Deppy Sijo.app` avoids the running `target/bundle` app.
- Final release build and local app/ZIP verification passed. Both bundle plist
  versions are 0.2.4, and app executable SHA-256 matches its ZIP entry
  (`f57028de27e549eb12d0a0a264d604d3e3d3421368c41b0b6caad5ebb299b731`).

No native physical Korean 2-set keyboard QA was run because this task did not
authorize an app restart. The test harness models event and physical-key
observations but cannot prove AppKit's exact live event order. The staged local
bundle is Developer ID signed; Apple notarization was not performed.
