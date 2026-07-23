# PR-OD02 summary

## Scope

Remove the eager application status-feed thread and periodic HTTP work without moving network I/O
into render or adding another runtime. The status feed remains independent from Connector state and
retains its existing bounded parser/cache contracts.

## Lifecycle

- Construction allocates only bounded channel/control state: no thread, HTTP agent, network,
  timeout, timer, or repaint.
- The first visible Home transition or explicit refresh intent starts exactly one standard worker.
- Refresh state is one coalesced bit and results are one latest-only snapshot.
- Leaving Home or hiding the viewport cancels the active round after at most the current bounded
  ten-second HTTP request. No inactive result schedules a repaint or another network request.
- An inactive worker exits after a 30-second idle TTL. The next explicit intent joins/reaps that
  handle before starting at most one replacement.
- Both idle expiry and cancellation before the first cycle publish `running=false` under the
  lifecycle lock before returning. Reactivation therefore joins the exact old handle before one
  replacement reserve; the sole activation edge cannot be lost in a pre-exit observation gap.
- Spawn failure and panic are sanitized and latched against frame-loop respawn until a new explicit
  refresh or inactive-to-active transition.
- Explicit shutdown and Drop signal the condition variable, join the worker, and release the latest
  payload. Tokio and new dependencies were not introduced.

## App integration

`eframe::App::logic` activates the feed only while Home is selected and the viewport is visible,
then drains immutable latest snapshots outside render. `shutdown_on_exit` stops and joins the feed
before the remaining application workers. The compatibility constructor name remains, but is now
zero-resource and does not spawn.

## Verification

- Status-feed focused tests: 22/22, including exact idle-expiry and pre-first-cycle reactivation
  races.
- App all-target check: passed on the integrated tree.
- App-scoped strict Clippy with `--no-deps -D warnings`: passed.
- Rustfmt and diff-check: passed.
- Dependency-inclusive workspace check and strict Clippy passed before the final race changes; the
  final integrated workspace rerun is a BG01 gate.
