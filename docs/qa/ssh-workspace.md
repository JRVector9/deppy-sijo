# SSH workspace implementation and verification — 2026-10-22

Branch: `feat/ssh-workspace-20261022` in
`/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022`.
This records source implementation and isolated investigation/test builds. No app
release, user server installation, external push, app launch or restart occurred.

Small durable records of the commands, exits, source/log hashes, sealed helper
pair and selected RED/GREEN result lines are in
[the evidence directory](evidence/ssh-workspace-20261022/README.md). The original
failed final cohort is retained separately from its fixed rerun. The label
`default-app` in the original runner means `--no-default-features`, not a test of
normal default SSH exposure.

Final product source commit: `8489d6532ae69ce0bd3ef035e24e5601a5b1ac8e`
(`feat: enable verified SSH workspaces in default builds`). The scoped integration
commits are `9e54d2b6` (test fixture lock), `8467a187` (OpenSSH compatibility),
`11b5eac1` (App/native/PWA integration), `be2cd502` (sealed package/dev assets),
and `8489d653` (default exposure). All subsequent changes are documentation.

## Six-stage implementation map

| Stage | Implemented scope | Primary source | Actual verification |
| --- | --- | --- | --- |
| PR1 | Typed Local/SSH identity, connection profiles, migration and legacy path boundary | `core/workspace_target`, `storage/workspace_target`, DB/identity | Core 5, storage 8 and adjacent workspace 48 regression tests; commit `0c8c48a8` |
| PR2 | OpenSSH destination parsing, transient askpass, trust, cancellation, resolved peer verification and trusted diagnostics | `ssh-transport`, `ssh-askpass`, App `ssh_connection` | Original transport/askpass 23; corrected 28 + actual loopbacks 2; concurrent exit127 20/20 cohorts |
| PR3 | Bundled static Linux helpers, consent and atomic install, stdio bridge, private supervisor, detach lease and proof-bound resume | `remote-host`, runtime SSH stream, App `remote_bootstrap` / `ssh_workspace_host` / `ssh_resume_store` | Native + two Linux runtime/helper cohorts; pinned Debian 12 static pair; real Host/Linux resume and consent |
| PR4 | Paged root-bound files, no-follow ancestors, CAS save, read-only Git status/working diff, exact RPC ownership and bounded observation | Helper files/Git, App `ssh_workspace_files` / `document_io` / observer | Helper/provider regressions, actual Host file/Git flows, queued ownership and observation App tests |
| PR5 | Shared SSH add popup, correlated trust/auth/install/folder flow, remote runtime ownership, agent metadata and authority badges | App `ssh_workspace_add` / runtime wrapper / sidebar / launcher; five locales | Focused App/wrapper cohort; offscreen eight-state normal/narrow renders; existing Local/GitHub full-App regression gate |
| PR6 | Same-session reconnect, fresh UI attachments, persistent PWA identity, long input/resize, confirmed close and final exposure gates | App SSH integration, runtime/PWA routing, packaging scripts and this QA report | Actual framed App/PWA/PTY close tests; packaging/deadline/trust/native archive QA; final whole-workspace gates below |

This map records implementation and exercised source behavior. Physical Mac
sleep/wake and live cloud-agent account compatibility remain unexercised. The
release version/build/signing step is outside this source-only task.

## Behavior and boundaries

- Add a workspace through **SSH server → connect → choose server folder → add**.
  Aliases, `user@host`, an `ssh` prefix and SSH URIs use OpenSSH config, agent and
  ProxyJump. Port and identity options are optional. Arbitrary commands/options
  in the destination are rejected.
- Trust and transient authentication prompts appear in the same popup. Each
  probe/helper/install phase verifies the actual host, port, account and host key
  before receiving a resume proof or helper bytes. Changed keys are not silently
  accepted. Server banners cannot supply authenticated diagnostics.
- Linux arm64/x86_64 helpers are bundled, size/hash/version/ELF checked, and
  installed atomically in the SSH account only after current explicit consent.
  No sudo, external helper download or new public server port is required.
- Workspace identity includes the verified host/account/key/canonical root and
  server workspace ID. Remote roots remain remote; legacy local path is empty.
  Local dotenv, API credentials, local processes, Mac file IO/watchers, structured
  Codex app-server initialization and local layout restoration stay outside SSH.
- A private per-root Linux supervisor owns PTYs independently of a bridge. A
  300-second detach lease permits exact-proof reattachment. App shutdown detaches;
  Close Workspace closes remote panes and waits for an exact ordered server
  acknowledgement. Deadline/connection loss reports **unconfirmed**, never a
  successful kill. Expired/foreign proofs do not restore or replay input.
- File lists are paged; reads and CAS/atomic writes are root bound. Every directory
  ancestor is opened with no-follow checks. Directory names retain Unix bytes.
  Read-only Git status and working-file diff run on the server and honor the
  selected root/subfolder and literal paths. Replies must match workspace,
  attachment, connection generation, server runtime and request.
- App providers use at most eight outstanding RPCs. The server RPC executor is
  bounded and separate from terminal input/heartbeat work. Retiring an owner
  completes its pending document/list/Git actions instead of retaining dead slots.
- Installed server agents are selected from server metadata. Sidebar and terminal
  context show actual authority, remote root and connection state. PWA reaches the
  same server session through the Mac gateway. Persistent server session identity
  is separate from a fresh UI attachment identity.
- Popup geometry follows the approved HTML: 560pt content width, 36pt inputs,
  34pt buttons, 8pt footer gaps, and scrolling inside the body. Trust/authentication
  controls and folder navigation share aligned rows. All five locales are present.

Remote PTYs have the SSH account's normal permissions; the selected file-service
root is not an operating-system sandbox. Native file mutations/open-in-Finder,
local history/transcript discovery, tracked model acknowledgements, and other
unimplemented remote operations fail explicitly. Git history, commit-file queries, staged App queries and mutation commands are not
implemented. Physical laptop sleep/wake and live user cloud-agent accounts were
not exercised; disconnect, timeout, stale-generation and reattachment behavior
were tested with isolated fixtures.

## Actual executed gates

| Gate | Actual result | Evidence |
| --- | --- | --- |
| Target migration/identity | Core 5 + storage 8 regression tests passed; adjacent workspace 48 passed | `0c8c48a8` |
| OpenSSH transport/askpass | Corrected 28 default tests passed; corrected strict whole-workspace Clippy passed | `ssh-transport-worker.md`; `/private/tmp/deppy-ssh-compat-final-unit-20261022.log` |
| Real OpenSSH security | Trust, malicious login banner and changed-key loopback passed separately | `/private/tmp/deppy-ssh-banner-loopback-green-20261022.log` |
| Runtime SSH / helper | Native and GNU Linux arm64/x86_64: runtime 20 and helper 15 library + 2 process tests passed each | `/private/tmp/deppy-ssh-linux-{arm64,x86_64}-pwa-20261022.log` |
| Static supported baseline | Both coherent musl helpers execute exact version on pinned Debian 12; archive tests 34 passed per architecture as UID65534 | `/private/tmp/deppy-ssh-musl-build-serial-20261022.log` |
| Static artifact validation | Both exact architectures, version, size/hash, no interpreter/dynamic dependencies passed; sealed manifest written | `target/remote-host/0.13.0` |
| Final corrected Host → OpenSSH → Linux | Register, browse, files/CAS/conflict/traversal/Git, Korean PTY input, resize, owner restart, same-session resume/no replay and kill passed with final static ARM helper; 1 passed | `/private/tmp/deppy-ssh-final-linux-host-20261023.log` |
| Final corrected helper install consent | Final sealed pair: no install before consent, stale consent ignored, current consent installs and connects; 1 passed | `/private/tmp/deppy-ssh-final-linux-install-20261023.log` |
| App focused stage18 | 62 passed, 0 failed, 3 opt-in tests ignored; wrapper 3 passed | `/private/tmp/deppy-ssh-{app-integration,wrapper}-stage18-green-20261022.log` |
| Actual App/PWA | Watch + ~5KiB Korean/emoji input + resize; A→B→A reused numeric session isolation; same-PTY reattach/new attachment/old-watch rejection/no replay passed | Included in the focused App cohort |
| Actual App remote close | Warm PTY termination, pre-admitted late spawn, empty cached snapshot and uncertain timeout passed | Included in the focused App cohort |
| UI render | Eight states × normal/narrow: 16 PNGs rendered; browse/trust/auth/ready and narrow layouts inspected | `target/ssh-ui-qa`; `/private/tmp/deppy-ssh-ui-render-final-20261022.log` |
| Packaging/deadline unit tests | 13 passed; sh/bash syntax checks passed | `/private/tmp/deppy-ssh-final-packaging-unit-20261022.log` |
| Original final architecture/dependency/i18n | All three commands passed in both original and fixed cohorts | Durable `gate-results.json` |
| Original whole-workspace + SSH | Failed: App 3343 passed / 2 failed / 42 ignored; xtask 15 passed / 1 failed | Source-guard locator, narrow GitHub popup and stale packaging-command expectation |
| Original no-default-features App | Failed: 3341 passed / 1 failed / 41 ignored | Same source-guard locator regression |
| Original strict/fmt | Failed: four strict Clippy findings and module-order formatting | Durable `gate-results.json`, `initial_final_cohort` |
| Fixed four-thread whole-workspace gates | Completed with one failed command: App 3332 passed / 13 failed / 42 ignored; xtask 19 passed | Missing test fixture lock caused reader-counter overlap; 12 later failures were mutex-poison cascades |
| Fixed no-default-features App / strict / fmt / boundary / deps / i18n | All passed; no-default App 3342 passed / 0 failed / 41 ignored | Durable `gate-results.json`, `fixed_final_cohort` |
| Targeted corrected agent detection / UI | Agent suite 71 passed / 1 ignored at four threads; UI 1 passed / 16 PNGs; two representative frames visually inspected and retained | [Durable UI evidence](evidence/ssh-workspace-20261022/ui-evidence.json) |
| Corrected final cohort | All ten commands passed: workspace App 3345 passed / 42 ignored; legacy App 3342 passed / 41 ignored; 0 failures | Before/after source digest matched: `917dfe6193e67ad9fb793156b759e1abd777c05457e440adc6cfd53dd9bb4737` |
| Corrected strict / fmt / architecture / deps / i18n / Python | All passed; Python 23 tests; strict whole-workspace Clippy has no warnings | Durable `gate-results.json`, `corrected_final_cohort` |
| Final corrected real Linux integration | Host and install-consent opt-ins both passed after the ten gates, on the same frozen source | `/private/tmp/deppy-ssh-final-linux-20261023.json` |
| Default exposure policy RED | Packaging 3 Python tests / 1 failure before the coordinated defaults: omitted SSH feature | `/private/tmp/deppy-ssh-default-exposure-red-20261023.log` |
| Normal default SSH exposure | All seven gates passed: App 3345 passed / 0 failed / 42 ignored; Python 23; strict/fmt/package/boundary/deps passed; App/strict omit `--features` | Matching before/after source digest `ae86f37cba63d251dab05e32c96c6687863db2d3bac4b32d3054edc343ce81fc` |
| Default-feature metadata | Read-only Cargo metadata passed: deppy-sijo 0.13.0, default features `[ssh-workspaces]` | Durable `gate-results.json`, `default_feature_metadata` |
| Owned fixture cleanup | Stop command passed; task-owned container and client/host keys removed | Durable `gate-results.json`, `fixture_cleanup` |

The focused count is not a whole-App count. The original final run completed and
found the failures above. The fixed four-thread run exercised the corrections,
then encountered separate agent-detection test infrastructure: the real-lsof
fixture launched two counted readers without taking the existing shared test
lock. The fixed runner completed; every other command passed. The subsequent
corrected run is preserved as a separate cohort; it cannot replace this
failed execution or establish physical sleep/wake or live cloud-account coverage.

The [Rust SSH design review](2026-10-22-rust-ssh-design-review.md) subsequently
found source-level compatibility gaps for OpenSSH host-certificate diagnostics,
the user's `FingerprintHash md5` setting, and preparation of askpass/helper assets
in `dev-run.sh`. Transport compatibility and development argv REDs are recorded
below; follow-up diagnostic/config-context and helper-resource REDs are also
preserved. Targeted corrected GREEN executions passed: 28 transport/askpass tests,
both real OpenSSH loopbacks, 23 Python tests and an isolated two-iteration real
helper-resource copy/refresh. All ten corrected full gates and both final real
Linux reruns passed with matching before/after source hashes. The review's
2ms idle-loop observation has no
measured CPU or performance result.

| Late compatibility check | Actual RED result | Evidence |
| --- | --- | --- |
| Transport/askpass default tests | Worker 19 passed / 2 failed: valid certificate binding and SHA256 argv; malformed certificate-primary rejection passed | `/private/tmp/deppy-ssh-compat-default-red-20261022.log` |
| Real OpenSSH loopback | 0 passed / 2 failed: CA-trusted host certificate metadata and configured MD5 prompt behavior | `/private/tmp/deppy-ssh-compat-loopback-red-20261022.log` |
| Development build argv | 2 Python tests / 4 failing debug/release subtests: missing askpass and `--locked` | `/private/tmp/deppy-ssh-dev-build-red-20261022.log` |
| `LogVerbose` diagnostic prefixes | Unit 0 passed / 1 failed; actual certificate loopback 0 passed / 1 failed | `/private/tmp/deppy-ssh-logverbose-{unit,loopback}-red-20261022.log` |
| `ssh -G` resolution context | 0 passed / 1 failed; actual scratch proof selected different accounts with/without the probe command | `/private/tmp/deppy-ssh-config-context-{proof,red}-20261022.log` |
| Development helper resources | 9 tests / 12 unittest failure entries; unmanaged-resource focused regression 1 test / 1 failure | `/private/tmp/deppy-ssh-dev-{assets,unmanaged}-red-20261022.log` |

The local date advanced to 2026-10-23 during this work; the existing 20261022
cohort filenames are retained. Durable exact commands, exits and log hashes are in
`gate-results.json` under `compatibility_regressions`; these REDs are separate from
the earlier whole-workspace cohorts. `compatibility_regressions.green` records
the corrected targeted results, while `corrected_final_cohort` records all ten
successful full gate commands. `final_linux_integration` records both opt-ins;
all share the verified matching source snapshot. The initial failed executions
remain preserved.

Linux architecture runs used disposable container fixtures; the x86_64 run used
QEMU. These counts establish exercised execution, with no CPU/memory benchmark
claim. The development resource proof and UI renders did not execute the App.

## Regression evidence and corrections

- Foreign resolved peers initially received hello/resume bytes. Actual peer tests
  failed, then passed after proof verification moved before all task IO.
- Server login banners can forge ordinary stderr diagnostics. A real OpenSSH RED
  proved the fingerprint confusion. OpenSSH `-E` now writes trusted diagnostics
  into a 0600 FIFO inside a private 0700 broker; stderr is separately discarded.
  The pipe bounds data without retaining raw on-disk diagnostics.
- Completed handshake rejection interrupted SSH before exit127 was observed;
  queued hello writes to a closed stdin also hid127 as generic IO. Actual REDs
  led to preserving completed-handshake exit status and draining BrokenPipe for
  at most250ms. The real partial-frame five-second watchdog remains. The former
  failing concurrent peer workload passed 20/20 cohorts (80 executions).
- A persistent server runtime token was reused as the UI owner. Reattach now has
  a fresh attachment token while RPC uses the stable server token. No-persist
  remote sessions initially lacked PWA UUIDs; stable scoped UUID projection fixed
  actual dashboard watch/input and A/B/reattach regressions.
- Same-path document recovery and queued reads could cross servers or reach Mac
  IO after owner/catalog loss. Immutable document transport ownership now guards
  load, save, queued actions and recovery. Exact positive Local ownership is
  required for queued workspace/Git/transcript native effects.
- Structured Codex start/catalog/rate-limit paths could initialize the Mac agent
  for SSH. Actual injected host-counter REDs now pass with local admission fenced.
- Retired RPC owners left eight slots permanently occupied. Scoped retirement now
  fails and removes only those exact owners' pending actions.
- Warm Close Workspace queued commands then detached immediately. Actual PTY RED
  proved lost close delivery. A second RED proved a pre-close admitted spawn
  survived. Bounded close rounds now await ordered server barriers and an empty
  mux; input/spawn admissions, including previously captured PWA sinks, are gated
  during close. The same deadline covers retries; timeout reports uncertainty.
- An old accepted barrier from a previous App instance could falsely acknowledge
  a fresh close. Actual framed RED now passes with a fresh nonzero random64-bit
  correlation per round; local runtime barrier allocation is unchanged.
- The approved remote file observation path initially had no active polling.
  Actual REDs now pass with one App-owned completion-relative, bounded roundrobin
  observer. Hidden/inactive/busy/closing and local owners do not poll.
- Package verification originally executed askpass version code before requested
  trust checks. Actual ad-hoc fixture RED now passes with trust-before-execution.
  A disposable native bundle and ZIP with both final static helpers passed deep
  signature/resource checks; rehashed helper tampering was rejected before code
  execution. See `/private/tmp/deppy-ssh-native-bundle-qa-20261022.log`. This is
  development-signature QA, not a Developer ID/notarization or release claim.

## Test-only infrastructure failures

These are not counted as product regression reproductions:

- Colima ENOSPC: only owned caches moved to verified shared `/Users` bind mounts.
  Initial `/private/tmp` mounts were VM-only. No global prune or unrelated asset
  deletion occurred.
- musl exposed an unresolved `renameat2`; the same Linux syscall/noreplace behavior
  now links statically. Archive permission tests must run unprivileged; running
  them as root initially bypassed chmod, then all34 passed per architecture as
  UID65534 without skipping checks.
- QEMU/Cargo twice stalled in a pre-exec futex with no rustc. Preserved process and
  partial JSON evidence in `target/remote-host-build-evidence-20261022`; stopped
  only the exact task containers. Archive test compilation with jobs1 completed.
  Build/baseline containers now have host-side45min/5min deadlines and exact-name
  cleanup. Helper compilation still uses jobs2.
- A PWA fixture once hung in its unbounded server join after its assertions. An
  executed sample did not prove a production runtime cause. Fixture cleanup now
  records/asserts the client interrupt and successful socket shutdown before
  independently closing its owned server endpoint and bounding the join. Current
  cohorts pass; this is not described as a production runtime fix.
- A fixed final four-thread App run exposed a pre-existing agent-detection test
  fixture missing `COMMAND_TEST_LOCK`: its two real-lsof readers overlapped an
  assertion in a guarded test, then poisoned the shared mutex for 12 later tests.
  Source review confirmed the unguarded caller. The one-line test-only correction
  passed the targeted 71-test agent suite and both later whole-App cohorts with
  the counter assertions and four-thread workload retained. No SSH product
  behavior or production timeout is changed.
- Isolated installation fixtures initially moved only a binary instead of its
  version directory and had root-owned parent directories. The installer properly
  refused inconsistent state; fixture setup/RAII restoration were corrected.
- Docker archive-copy into the fixture's home tmpfs twice failed to resolve an
  existing parent. Docker exec stdin installed the exact final helper hash instead.

## Repeat commands and release boundary

Run these only in the canonical branch; root coordinates App Cargo with jobs2.

```sh
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo --features ssh-workspaces ssh_ -- --test-threads=4
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo --features ssh-workspaces workspace_runtime_client -- --test-threads=4
python3 -m unittest scripts/tests/test_dev_build.py scripts/tests/test_prepare_remote_host.py scripts/tests/test_ssh_packaging.py scripts/tests/test_remote_host_deadline.py scripts/tests/test_package_trust.py
```

`DEPPY_SSH_FIXTURE` explicitly selects a disposable fixture JSON for the two ignored
Linux tests; `DEPPY_SSH_HELPER_ARTIFACTS` selects the final sealed pair for consent.
The offscreen popup test is opt-in and does not launch Deppy. The fixture used here
was removed after the final Linux tests; the stop command exited0. Future fixture
cleanup uses `bash scripts/ssh-linux-fixture.sh stop <private-state-directory>`,
without pruning global Docker resources.

Source version remains `0.13.0` for these investigation builds. A delivered app must
increase the canonical version above the latest shipped version from every branch,
rebuild/seal both helpers for that version, and verify the binary and both bundle
version fields. This source task does not grant permission to restart the app.
