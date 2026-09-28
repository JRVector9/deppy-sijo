# AI terminal input harness

The headless trace harness is `crates/app/src/ui/workspace/tests/ime_input_harness.rs`.
It feeds ordered `egui::Event` batches through the real `WorkspaceUi::show_with_input`
renderer, injects native printable key observations where needed, and asserts
the exact session ID and PTY bytes emitted per frame.

Run the focused input trace suite:

```sh
cargo test -p deppy-sijo --bin deppy-sijo ime_input_harness -- --nocapture
```

Current traces cover:

- Enter during Korean composition, an empty preedit, then a late Commit after
  switching sessions: the old session receives the completed syllable, optional
  punctuation, and Enter in order; the new session receives no duplicate.
- A new composition after that switch reaches the new session.
- Commit, Enter, and the next key separated by UI frame boundaries retain order.
- A Commit that never arrives releases the last visible syllable before Enter
  when the bounded pending period expires.
- Native punctuation bundled with an old Commit is not replayed into the new
  session. Paired `Key` and `Text` echoes are suppressed too; physical keys and
  text independently entered in the new session remain eligible for input.
- A runtime focus acknowledgement leaves detached input waiting for its Commit.
  Queued punctuation overlapping that Commit is emitted once. A clipboard read
  completed after detachment remains ordered behind the original Enter. A read
  requested before Enter retains its bytes even if they match Commit punctuation.
- TextEdit-owned IME events do not resolve a detached terminal submit. Physical
  punctuation typed after the old Enter remains in the new pane, even when its
  character matches punctuation inside the old Commit. The old Enter still
  expires after two seconds while TextEdit owns the keyboard.
- A Commit that arrives after timeout, together with its paired Key/Text
  echoes, cannot enter the new session.
- A full Commit Text echo is removed even when a separate physical key is
  typed in the new pane; that physical key still reaches the new pane. The
  echo is also recognized when its paired Key occurs between Commit and Text.
- On platforms without AppKit physical-key observations, a bare matching
  suffix Text next to the old Commit remains available to the new pane; a full
  echo or a Key+Text pair is still attributed to the old Commit.
- Leaving the workspace flushes any detached pending input.
- Navigation without active composition reaches the PTY unchanged.

Add new reports as a trace that first fails with the observed PTY byte sequence,
then make the smallest source change that passes it. Keep session IDs in the
assertions; a correct byte sequence sent to the wrong session is still a bug.

The harness does not synthesize AppKit's actual Korean 2-set keyboard events,
native modifier flags, or clipboard service timing. Physical keyboard QA
still requires a separately authorized app restart and a real macOS input source.

References: [Warp Korean Enter issue](https://github.com/warpdotdev/warp/issues/8919),
[cmux IME interception regression](https://github.com/manaflow-ai/cmux/issues/3762),
[cmux narrower fix](https://github.com/manaflow-ai/cmux/pull/3867), and
[cmux modifier-state issue](https://github.com/manaflow-ai/cmux/issues/2949).
