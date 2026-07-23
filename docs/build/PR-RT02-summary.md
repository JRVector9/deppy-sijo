# PR-RT02 — Bounded Warm Runtime Shutdown

## Scope

Bound background workspace shutdown ownership without adding a timer, polling loop, detached
reaper, async runtime, or second production path. RT02 preserves the existing Active/Warm/
Suspended behavior while making shutdown concurrency, handle ownership, idle scheduling, and live
session protection explicit.

## Bounded shutdown registry

- `PendingShutdownRegistry` admits at most two named `workspace-shutdown` threads. Admission first
  joins and removes finished handles; a full registry leaves the candidate runtime resident in the
  warm pool instead of creating another thread.
- Each accepted shutdown remains registry-owned until it is joined. Activating the same workspace
  joins its prior shutdown before rebuilding the runtime, explicit app shutdown joins every
  remaining handle, and `Drop` provides the final join guarantee.
- A panic-safe completion guard publishes an atomic flag before requesting the one repaint after
  `RuntimeHost::shutdown` returns. Logic observes that flag with acquire ordering, joins the handle,
  and retries a capacity-deferred eviction even in the brief interval before
  `JoinHandle::is_finished()` changes. There is no detached fallback, retry queue, timer, or
  automatic external operation retry.

## Transition-maintained idle deadline

Warm-pool transitions maintain one stored earliest eligible idle deadline. Adding, removing,
reactivating, suspending, or changing the eviction eligibility of a warm runtime recomputes that
deadline. A changed non-empty deadline schedules one `request_repaint_after`; an unchanged
deadline schedules nothing. Logic compares the stored deadline with the current instant and does
not scan the warm map each frame.

When shutdown capacity is full, the idle deadline is cleared and eviction is marked deferred.
Completion wake, handle reap, and retry restore the deadline or immediately process an already-due
candidate. This keeps idle operation free of periodic polling and repaint while ensuring deferred
work is not stranded.

## Live-session protection

- Size-based warm eviction excludes every runtime with a live session.
- Idle suspension considers a live runtime only when it is proven to contain reconstructible,
  prompt-waiting shell sessions. Pending shell/agent spawns, an unobserved initial mux, detected or
  unclassified agents, child processes, missing resource evidence, high CPU, and high RSS all keep
  the runtime warm.
- Immediately before shutdown, the final runtime-event drain repeats the live/idle-shell check. If
  eligibility changed, eviction is canceled, the runtime is restored to the warm pool, and drained
  lifecycle events are retained for replay.

## Verification

- Shutdown-registry focused tests: 3/3 passed, covering the two-thread cap, completion-before-wake
  ordering, finished-handle reap, `join_all`, and the `Drop` join guarantee.
- Warm deadline focused test: 1/1 passed, covering first scheduling, unchanged-deadline suppression,
  deadline removal, and an already-due zero-delay wake.
- Existing pure warm-candidate and live-session guard coverage is retained; RT02 does not weaken
  those pre-shutdown eligibility checks.
- `cargo check -p deppy-sijo --all-targets`, literal strict app Clippy with `-D warnings`, full
  workspace rustfmt check, and `git diff --check` passed on the integrated tree.

## Resource result

Background workspace shutdown can retain at most two shutdown threads and two registry handles.
Warm idle scheduling retains one optional deadline and produces one wake only when that deadline
changes; it adds no periodic timer, network request, process, queue, or per-frame warm-map scan.
