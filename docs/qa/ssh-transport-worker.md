# SSH transport worker verification — 2026-10-22

PR2 implementation in `/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022`.
The root worker owns `docs/CODEX_HANDOFF.md`, workspace manifests/lock and commits.
No Deppy application was launched/stopped/restarted. No release build or app artifact
was delivered. Workspace version remains 0.13.0 for this investigation/test build;
release must independently increase the version and verify all bundle values.

## Final integration note (2026-10-23)

The chronological entries below retain earlier default-off and pending states.
The final authoritative results are [the integration QA report](ssh-workspace.md)
and [Rust SSH design review](2026-10-22-rust-ssh-design-review.md).
Research added strict host-certificate primary parsing, SHA256/LogVerbose output
options and identical `-G`/execution command context. Actual final default
transport/askpass tests:28PASS,2opt-in ignored; both actual OpenSSH loopbacks:2PASS.
The corrected whole workspace and latest real Host/install tests passed before
coordinated default exposure. Normal-default App subsequently3345PASS0FAIL42ignored;
all seven final exposure gates passed and are recorded separately, without replacing earlier
RED evidence. This remains source/investigation work, not an App release.

## Current objective and completed work

- Added `ssh-transport`: one owned background worker runs real `/usr/bin/ssh`, resolves
  effective host/account/port using bounded `ssh -G`, preserves OpenSSH config,
  aliases, identity selection, agent and ProxyJump, and exposes bounded binary stdio.
- Destination parsing supports alias, user@host, ssh prefix, ssh URI, IPv6 and URI
  ports. Extra options/commands, password URI and shell/control syntax are rejected.
- Enforced StrictHostKeyChecking=ask, UpdateHostKeys=no, VerifyHostKeyDNS=no,
  ControlMaster/ControlPersist off, encrypted keepalive15s/count3 plus TCP keepalive,
  no agent/local forwarding and a fixed remote
  command. OpenSSH alone owns host-key validation and known_hosts writes.
- Added bounded private askpass IPC with random capability, 0700 ephemeral directory,
  0600 socket, temporary zeroized response storage, explicit host trust versus
  authentication confirmation, no response in argv/environment/DB/log/stderr.
- Nonblocking pipes and finite per-tick work keep stderr draining and cancellation
  progressing under floods. Request/output queues are bounded. Stalled output and
  stdin writes have deadlines. Idle runtime reads remain alive until EOF/cancel;
  bootstrap may opt into `SshReader::set_read_timeout`.
- Child process groups include ssh/ProxyJump/askpass. `waitid(WNOWAIT)` retains the
  child's PID ownership until process-group kill/reap. UI Drop signals cancellation
  without joining. `Ended(Exit)` is clean child0; `RemoteExit(code)` is nonzero.
- `SshStream::split` yields Send Read/Write adapters for PR3. Flush waits for prior
  bytes to reach the child pipe. Input is never automatically resent.
- Added typed immutable-version helper/probe/install commands, strict version and
  digest parameters, staging/checksum/architecture/version verification and atomic
  install. Artifact size/consent validation belongs to remote_bootstrap. Probe emits
  four lines: OS, architecture, actual account, HOME.
- Added app `ssh_connection.rs`: generation invalidation, bounded polling, exact
  pending-auth response gating and helper-account/fingerprint readiness checks.
- Package scripts now build/copy/strip/sign askpass; verifier checks executable,
  signature, signing team, archive identity and companion version.

## Modified files

- `crates/ssh-transport/{Cargo.toml,src/lib.rs,src/destination.rs,src/worker.rs}`
- `crates/ssh-transport/tests/{destination.rs,worker.rs,openssh_loopback.rs}`
- `crates/ssh-askpass/{Cargo.toml,src/lib.rs,src/main.rs}`
- `crates/ssh-askpass/tests/{protocol.rs,companion.rs}`
- `crates/app/src/ssh_connection.rs`
- `scripts/{package-macos.sh,verify-macos-package.sh}`
- This QA note. Root registers crates/app modules and dependencies separately.

## Actual RED/GREEN evidence

Initial compiled fixture REDs were missing destination/worker/askpass protocol
behavior (not compile errors). Additional behavioral REDs then GREEN verified:

- Authentication confirmation must not be classified as host trust.
- Full stdout consumer queue must expire instead of keeping an owned child forever.
- Silent healthy runtime must not expire at the I/O backpressure deadline.
- Nonzero ssh exit is not successful probe; failed `ssh -G` cannot supply identity.
- Flush must wait for actual child-pipe progress, beyond queue acceptance.
- A child that closes stdout but stays alive must be reaped after a bounded250ms
  EOF grace, so stream readers receive EOF without indefinite wait.
- Real OpenSSH10.3 first-host prompt uses `key fingerprint is: SHA256:`; parser now
  accepts both documented OpenSSH prompt forms and still requires fingerprint and
  yes/no confirmation.

Executed final verification (all exit0):

```sh
CARGO_TARGET_DIR=target-ssh CARGO_BUILD_JOBS=2 cargo test --locked -p ssh-transport -p ssh-askpass
# 19 passed; the explicit loopback test is ignored in the default suite.
CARGO_TARGET_DIR=target-ssh CARGO_BUILD_JOBS=2 cargo build --locked -p ssh-askpass
DEPPY_TEST_ASKPASS="$PWD/target-ssh/debug/deppy-ssh-askpass" CARGO_TARGET_DIR=target-ssh CARGO_BUILD_JOBS=2 cargo test --locked -p ssh-transport --test openssh_loopback -- --ignored --nocapture
# 1 passed against actual local OpenSSH/sshd, not a user server.
CARGO_TARGET_DIR=target-ssh CARGO_BUILD_JOBS=2 cargo clippy --locked -p ssh-transport -p ssh-askpass --all-targets -- -D warnings
sh -n scripts/package-macos.sh scripts/verify-macos-package.sh
git diff --check
```

The real loopback fixture generates throwaway keys, isolated ssh config and
known_hosts under a private `/tmp/deppy-loopback-*` directory and listens only on
127.0.0.1 at a temporary port. It proves explicit first-host approval through the
actual bundled askpass, exact effective host/account/port, final OpenSSH SHA256
observation, fixed probe stdio and changed-key refusal with no overwrite/prompt.
It cleans its own sshd/ssh processes and files. It never reads/writes `~/.ssh`.

The app controller's three behavioral tests also ran RED then GREEN through a
standalone rustc test harness linked to the actual transport crate (not full app):

```sh
python3 - <<'PY'
from pathlib import Path
import subprocess
libs = list(Path('target-ssh/debug/deps').glob('libssh_transport-*.rlib'))
assert len(libs) == 1
subprocess.run(['rustc', '--edition=2024', '--test', 'crates/app/src/ssh_connection.rs',
                '--extern', 'ssh_transport=' + str(libs[0]), '-L', 'dependency=target-ssh/debug/deps',
                '-o', 'target-ssh/debug/ssh-connection-tests'], check=True)
subprocess.run(['target-ssh/debug/ssh-connection-tests'], check=True)
PY
```

## Failed approaches and remaining integration

A 200ms fixture connection deadline could expire during Python startup/`ssh -G`,
before the asserted main-child PID existed; the intended cancellation/main-child
case now uses 1s. The shorter deadline was not counted as a product failure.
Initial clippy found two collapsible-if warnings; both were fixed and clippy reran.

Full app/runtime integration, Linux installation of the actual helper, PR3 partial
handshake deadlines/liveness, UI screenshot QA and release signing/notarization
are separate gates; PR2 unit/loopback results do not claim those passed.
No real server credentials were used. The root must review source, integrate UI
and runtime, update the central handoff, and create the intended scoped commit.

Next commands: run the verification block above from the feature worktree, then
root-owned full app tests after UI/runtime module integration. Preserve default-off
`ssh-workspaces` until the complete end-to-end gates pass.

## Authentication readiness and app adapter follow-up

The host must defer the 5s helper protocol handshake until SSH authentication
finishes. `SshWorker::authenticated()` is a nonblocking historical readiness bit;
it is set only after the bounded diagnostic parser sees authenticated login and
the final destination key at remote command startup. Drain terminal events first.
The fake worker assertion ran RED (readiness absent), then GREEN. An initial
fixture had an invalid 45-character fingerprint and was corrected to 43; that
fixture failure is not counted as product RED. The existing real loopback proof
now also asserts false readiness at host trust and true readiness after login.

Actual follow-up verification: normal transport/askpass **20 PASS**, explicit
isolated real OpenSSH **1 PASS**, strict two-crate all-target clippy exit 0.
Commands are the same verification commands above. No user server or app launch.

`workspace_runtime_client.rs` is the app-owned Local/Remote/Offline adapter.
Offline construction never creates a local runtime. Commands fail Disconnected,
owned requests remain recoverable and subscriptions are already disconnected.
Remote inspection/output-reply APIs fail with explicit typed unsupported errors;
explicit guarded input delegates, automatic/tracked input is rejected by runtime.
Remote shutdown calls the public nonblocking `disconnect(&self)`; it never asks
the helper to terminate remote sessions. The initial offline/local behavioral
tests ran RED, then three tests ran GREEN against the actual runtime rlib. The
third uses the real SSH runtime over a local UnixStream fixture without any PTY,
shell or credential. This direct rustc harness is not a full App Cargo gate.

The direct harness links runtime plus its matching anyhow/uuid/secret rlibs from
`target-remote/debug/deps`; dependency hashes come from Cargo fingerprint files
to avoid mixing different feature builds. An earlier stale runtime artifact
lacked the newly added disconnect method; `cargo build --locked -p runtime --lib`
in the runtime worker's target refreshed the normal rlib before the successful
test. An initial mismatched secret dependency was a harness setup error, not RED.

Remaining worker scope is the pure remote file/Git adapter and explicit remote
document revision. The root owns the central handoff and App dispatch wiring.

The explicit document revision is now a Copy enum: local metadata remains in the
Local variant; Remote stores a scope digest and the helper's metadata/content
digest. Local save rejects a remote revision before touching a local path. The
new behavioral assertion ran RED (Conflict instead of TargetMismatch), then
GREEN. A direct harness includes the real document_io.rs and lazy_worker.rs and
links cached uuid/tracing/libc/egui/egui_kittest rlibs. All document tests ran:
**17 PASS / 3 pre-existing ignored measurement tests**. The first harness lacked
egui dependencies and was corrected before RED; compile failure is not RED.

Provider stubs and five tests are frozen for the root-owned App Cargo RED run.
No provider implementation has been claimed tested yet.

## Remote file/Git provider completed (source and targeted App tests)

Root's actual App stage3 cohort executed all five stub assertions and each failed
for the intended missing behavior (`/private/tmp/deppy-ssh-app-integration-stage3-red-20261022.log`).
After implementation and added boundary cases, the root stage4b App cohort ran
**8 provider tests PASS** (`/private/tmp/deppy-ssh-app-integration-stage4b-20261022.log`).
The larger stage4b cohort was 30 PASS / 1 unrelated host typed-error RED; do not
claim that full cohort passed. Root still owns broader App regressions/Clippy.

`ssh_workspace_files.rs` has no filesystem/process calls. It maps lexical remote
Unix bytes into fixed typed service requests, accumulates bounded directory
pages, preserves non-UTF8 listing identity, refuses cursor replay/digest changes,
duplicate names and excess bytes, and prevents walking symlink entries. A reply
fence matches target identity, runtime instance, generation and request ID before
decoding. Remote document digest/size and metadata are checked, and revisions are
scoped to host/port/account/key/workspace/root plus exact raw relative file path.
Changed/deleted writes become Conflict; another target/file stamp is rejected.
Git porcelain is bounded, validates complete NUL records and rename source
records, rejects lossy action paths and limits rows; working diff uses the
existing pure unified parser. No local Git runs for remote requests.

Helper schema limitations remain explicit: Branch/Commit diff, native file tree
mutations/Finder/clipboard and local watches are unsupported rather than routed
locally. The helper's 256KiB document limit returns ContentTooLarge without
inventing a byte length. App dispatch/correlation integration is a separate gate.
The worker's isolated subfolder Git proof found that porcelain paths were repo
relative and could expose outside workspace paths; root assigned that helper
fix to the remote-host worker. This adapter does not claim that helper fix done.

## Static Linux helper packaging preparation

Added `scripts/build-remote-host.sh`, `scripts/prepare-remote-host.py`, and two
Python test files. Actual RED then GREEN: validator 5 tests (14 failing boundary
subcases at RED) and packaging argument routing 2 tests (5 failing assertions at
RED); final **7 PASS**. `sh -n` on build/package/verify scripts and diff check exit0.

Exact test command:

```sh
python3 -m unittest scripts/tests/test_prepare_remote_host.py scripts/tests/test_ssh_packaging.py -v
sh -n scripts/build-remote-host.sh scripts/package-macos.sh scripts/verify-macos-package.sh
```

The builder pins the cached official Rust multiarch image digest
`rust@sha256:1f0dbad1df66647807e6952d1db85d0b2bda7606cb2139d82517e4f009967376`,
asserts rustc 1.96.1, builds native-per-architecture musl targets with host-visible
CARGO caches, and checks each binary's exact --version. Every mount is verified
both ways with a random host sentinel before building. Each output executes on
the pinned Debian 12 slim baseline image
`debian@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8`.
These image digests were actually read through Docker metadata commands; the
builder has not yet run. The original bookworm Rust tag was verified available
but replaced with the cached pinned builder to reduce Docker disk pressure.

The validator requires exactly both known targets, fixed filenames, app version,
size, SHA256, valid ELF64 architecture/program headers, no PT_INTERP or DT_NEEDED,
no symlink resource and bounded manifest/binary reads. It copies a private stage
and publishes immutable resources. Only explicit `package-macos.sh --features
ssh-workspaces` enables Cargo's feature and copies those resources; plist and
archive checks enforce the same selection. Tests stop at a fake Cargo boundary,
so no app build/signing/package was run by packaging tests.

Remaining: actual musl builds and Debian 12 execution gates after the remote
worker's exact task-cache disk recovery; root's full App gate; release version
bump/signing/artifact verification only when release is authorized. No app
launch/restart, user server access, package delivery or release claim.

## Actual static musl build investigation and host review

The first actual arm64 musl build reached linking and failed because the existing
storage archive code called the unavailable musl libc `renameat2` symbol. Log:
`/private/tmp/deppy-ssh-musl-build-20261022.log` (exit101). Root applied the Linux
kernel syscall portability fix preserving RENAME_NOREPLACE. The next actual
arm64 helper release build linked and its archive test executable ran **33 PASS /
1 FAIL** under root: the chmod0555 permission fixture cannot deny deletion to
UID0. This is not a product failure or a passing cohort. Log:
`/private/tmp/deppy-ssh-musl-build-retry-20261022.log` (exit101).

The builder now compiles the exact storage test executable with Cargo JSON
artifact output, then runs it on the pinned Debian12 baseline as UID/GID65534
with a private tmpfs /tmp. It also executes each helper's exact --version on that
baseline. Actual rerun is in progress at
`/private/tmp/deppy-ssh-musl-build-nonroot-20261022.log`; no final pass claimed.
The mount sentinels, apt archives tmpfs, Rust1.96.1 assertion, musl target download
and host cache visibility all actually passed on arm64. Both-architecture
artifact sealing remains pending.

Read-only Host review found that peer equality was checked after helper hello
or install I/O. Root accepted a pre-task verification requirement so a fresh SSH
phase cannot send a resume proof or write an artifact before checking the exact
expected resolved host, port, account and fingerprint. New owned
`ssh_workspace_host_peer_tests.rs` calls real Owner.run with isolated fake SSH
processes, real private resume storage, and captured helper stdin. Two tests cover
changed host/account/port and changed key. A third uses a real runtime service
behind a Unix socket bridge to reject helper metadata architecture inconsistent
with the fixed uname probe. These tests are source-coherent/frozen for root's
next actual App RED cohort; only embedded Python syntax/rustfmt/diff checks have
run, not the Rust assertions. Root owns host.rs and module registration.

Root's actual App stage6 RED ran the two peer-change tests and both failed at the
intended assertion: captured helper stdin was nonempty before expected identity
verification. Log `/private/tmp/deppy-ssh-app-integration-stage6-red-20261022.log`.
The metadata fixture failed earlier with zero handler calls, so that result is
not an architecture-validation RED. A standalone actual macOS Rust socket proof
confirmed accepted sockets inherit the nonblocking listener flag: initial read
WouldBlock(35), explicit blocking then Ok(1). The fixture now explicitly selects
blocking I/O and 3s read/write timeouts; actual corrected metadata RED is pending.
Root owns production pre-task proof guard and attachment-token fixes. The new
peer test module remains frozen for the next root cohort.

Actual static build update: arm64 helper release links, reports exact version on
pinned Debian12, and **34 archive tests PASS / 0 failed / 0 ignored** as UID65534
on that baseline. x86_64 release compile is still active under QEMU with jobs2;
Docker stats showed about1.6GiB task memory. No two-architecture seal yet.

Corrected fixture actual App stage7 ran metadata handler once and then failed at
the intended assertion: an x86_64 probe accepted aarch64 helper metadata and
published Connected. Root's log is
`/private/tmp/deppy-ssh-app-integration-stage7-red-20261022.log`. Both peer-change
regressions now PASS after root's pre-task expected-peer proof guard. Root added
the architecture comparison as the GREEN candidate; final actual rerun pending.
Read-only re-review confirms both helper/resume and install writer use the guard.
Root also reported actual full Host E2E1PASS with the new arm64 static helper;
root owns that fixture/log and the claim's exact scope.

Added a fourth Host regression for the post-install expiry branch, source-frozen
for root RED: real Owner.run receives helper127, emits InstallRequired, receives
one explicit generation-scoped install intent, validates a synthetic64byte ELF
artifact/digest from isolated Resources, writes exactly64bytes and accepts the
fixed ready marker, then receives a real typed ResumeExpired handshake rejection.
It asserts SessionExpired, exact proof removal and preservation of another
workspace proof. A 5s cancellation watchdog bounds fixture failures. This test
has not executed yet; root owns the production error-clear correction.

Root actual App stage8: architecture regression now PASS, peer-change2PASS,
post-install expiry regression fails exactly at the final exact-target proof
removal assertion. All prior assertions (one consent, four phases, SessionExpired,
other-target proof retention) passed. Log:
`/private/tmp/deppy-ssh-app-integration-stage8-red-20261022.log`. This is the
intended behavioral RED; root will centralize typed expiry handling. The larger
cohort38PASS/4FAIL/3ignored also contains three independent App Server lane REDs;
do not describe it as a passing full App suite.

Packaging explicit-feature guard: added a behavioral test that both the default
package invocation and `--features ssh-workspaces` pass `--no-default-features`
to Cargo. Actual RED reported two missing flags in
`/private/tmp/deppy-ssh-package-defaults-red-20261022.log`; both build branches
now pass the flag. Actual rerun of
`python3 -m unittest scripts/tests/test_prepare_remote_host.py scripts/tests/test_ssh_packaging.py -v`
passed **8/8**. These tests use a fake Cargo that stops before any build, signing,
package delivery, or app launch. Root-owned final delivery/version gates remain.

Stage9 expiry fixture investigation: the current compiled App test binary
`target/debug/deps/deppy_sijo-891456d9bb4e82b1` passed the exact post-install expiry
test 13 consecutive times, but a concurrent four-peer-test cohort reproduced an
earlier approvals0 failure on repetition5 (first4 cohorts passed). No timing
workaround was applied. Test assertions now include only sanitized `Owner.run`
error enum and completed phase count so the next root build can identify the
pre-install failure. A possible missing-helper127/early-handshake cancellation
race is a hypothesis, not yet a confirmed cause. Peer source coherent/frozen for
root stage10. Both changed-peer tests and metadata architecture passed in the
reproducing cohort; this is not a passing full App claim.

The x86_64 static build remains active, now compiling storage after its earlier
terminal/dependency work. Runtime PWA UUID projection changed during this build,
so any initial sealed pair is an investigation artifact only. Both architectures
must be rebuilt after runtime source freeze before claiming a final coherent
pair. No macOS package or user delivery has been created.

Stage10 repeated concurrent peer tests reproduced the earlier failure with the
new sanitized evidence: `Err(ConnectionFailed), completed_phases=2`, before any
InstallRequired UI/consent. Log
`/private/tmp/deppy-ssh-peer-concurrency-stage10-20261022.log`. Runtime owner
confirmed that a completed hello error invoked interrupt before OpenSSH's127
could be recorded and owns its deterministic RED→GREEN correction. This is not
a UI scheduling workaround.

Adjacent transport regression actual RED: authenticated helper closes stdin,
receives queued hello attempt, then exits127; old worker published Ended(Io).
Log `/private/tmp/deppy-ssh-pipe-exit-red-20261022.log`. On BrokenPipe the worker
now stops stdin writes and drains remaining diagnostics/output/status for at
most250ms, preserving actual127 while cancellation remains checked every2ms.
A live child still fails Io boundedly and cancel remains prompt. Actual full
`CARGO_TARGET_DIR=target-ssh CARGO_BUILD_JOBS=2 cargo test --locked -p ssh-transport -p ssh-askpass`
passes22tests (17worker), one loopback ignored by default. Strict all-target
Clippy for both crates passes; explicit isolated OpenSSH loopback passes1.
Logs `/private/tmp/deppy-ssh-pipe-exit-{green,clippy,loopback}-20261022.log`.
Source coherent/frozen for root App stage11; no runtime API changes.

To unblock the parent's actual install-consent E2E while x64 archive test-only
dependencies compile, created an investigation-only pair at
`target/remote-host/0.13.0-consent-fixture`. Both binaries actually executed exact
`deppy-remote-host 0.13.0` on pinned Debian12, and prepare-remote-host validated
static ELF architecture, exact size and SHA before publishing the manifest.
ARM8563896bytes SHA256b73d5e358ffb97d9c3a67f0d44512d8b2fdeecc0f9cc6b1b3be8c5a38e95f1dd;
x64 9040424bytes,
SHA256aee8ba9fceb55a8429be6793bff437c783c9b57d61d14a4c3b1805c9ebed1cc7.
The ARM source predates UUID/hello-interrupt corrections, so this pair is NOT a
final coherent release artifact. x64 archive baseline tests are not yet claimed
passed. Original full build script continues to gate its separate default
output; final incremental rebuild of both architectures follows runtime freeze.
Parent E2E input:
`DEPPY_SSH_HELPER_ARTIFACTS="$PWD/target/remote-host/0.13.0-consent-fixture"`.

Post-fix actual concurrency verification used the root stage11 compiled binary:
`target/debug/deps/deppy_sijo-891456d9bb4e82b1 ssh_workspace_host::peer_tests:: --nocapture`
repeated20times. **20/20 cohorts PASS,80/80 test executions**. This is the same
four-test concurrent workload that failed repetition5 twice before the runtime
completed-error interrupt and transport BrokenPipe corrections. Log
`/private/tmp/deppy-ssh-peer-concurrency-stage11-20261022.log`. No fixture timing
relaxation was added. Parent's stage11 whole SSH-filtered App cohort also reports
51PASS/0FAIL/3ignored; that is distinct from a whole-App-suite claim.

Security review found and actually reproduced a second peer-proof boundary issue:
a real isolated sshd login Banner can imitate SSH diagnostic lines on stderr and
make old HostKeyObserved report the server-supplied synthetic fingerprint. Actual
temporary loopback RED is `/private/tmp/deppy-ssh-banner-proof-red-20261022.log`;
the promoted exact-real-host-key regression RED is
`/private/tmp/deppy-ssh-banner-regression-red-20261022.log`. A separate actual
OpenSSH experiment verified that `-E` keeps Banner text on stderr and puts the
real fingerprint in the diagnostics stream.

The worker now supplies `-E` with a0600 FIFO in its0700 private broker directory.
The reader opens with O_NOFOLLOW/O_NONBLOCK before SSH starts. Kernel pipe
capacity bounds raw diagnostics without storing an on-disk log; each loop drains
bounded chunks. Server stderr is drained/discarded independently and never
contributes host-key/authentication proof. FIFO cleanup joins existing owned-child
and private-broker cleanup. Legitimate fake SSH scripts in transport tests, Host
peer tests and the root's single cfg(test) typed-rejection script now write to
the supplied -E stream. Host production and public transport API are unchanged.
A new fake stderr-only spoof regression stays unauthenticated.

Actual GREEN:23default transport/askpass tests pass (18worker), strict2crate
all-target Clippy0, and explicit real OpenSSH trust+malicious-Banner+changed-key
loopback1PASS. Logs:
`/private/tmp/deppy-ssh-banner-regression-green-20261022.log`,
`/private/tmp/deppy-ssh-banner-loopback-green-20261022.log`,
`/private/tmp/deppy-ssh-banner-clippy-20261022.log`.
Source coherent/frozen for root's next App cohort; actual App fixture-adapter
rerun remains parent's next gate. No shared Docker fixture or user SSH files
were touched by the security proof.

Static x64 investigation correction: the earlier archive-test compile log stopped
progressing. Actual Docker stats fell to0.02% CPU; ps showed Cargo parent and
pre-exec child both in futex_wait for over10minutes, no rustc. Preserved ps and
partial Cargo JSON at `target/remote-host-build-evidence-20261022`, then killed
only that exact task container. Original build exited137; x64 archive PASS was
never claimed. Warm coherent retry runs session58333,
`/private/tmp/deppy-ssh-musl-build-final-20261022.log`. ARM current runtime rebuilt
in9.62s, Debian12 exact version and34archive tests PASS again. x64 current runtime
helper rebuilt in2m01s; archive compile is progressing beyond the prior stall.
Parent separately reports actual install-consent Linux E2E1PASS using the sealed
investigation pair; parent owns that log and assertion scope.

Added host-side per-container deadlines so a QEMU/Cargo stall is bounded:
`scripts/run-remote-host-container.py`, wired into build-remote-host.sh with
45min build /5min baseline commands. Cleanup names must match this builder's
random task namespace and only that exact container is removed. Active retry
also has a host watchdog attached to its exact verified task container ID.
Behavioral fake-Docker RED: hung command exceeded the harness bound; GREEN:
11total packaging/validator/deadline tests PASS. Deadline tests cover timeout,
exact owned cleanup, completed status preservation, and rejection of unrelated
container names. A first GREEN run used an unrealistic50ms deadline for a normal
process and failed that fixture under load; normal completion now gets2s while
the hung case still has100ms. Logs
`/private/tmp/deppy-ssh-build-deadline-{red,green}-20261022.log`. Shell syntax and
diff checks pass. No real Docker container was started or removed by these tests.

Final coherent static pair COMPLETE. After a second parallel Cargo/QEMU pre-exec
stall (ps and JSON preserved as retry evidence), the archive test compile now
uses `--jobs 1`; helper builds remain jobs2. The serial retry finished the x64
test binary in4m50s and the complete script exited0. Both architectures execute
exact `deppy-remote-host 0.13.0` on pinned Debian12; each runs34archive tests with
0failed/0ignored as UID65534 (68PASS total), including the permission failure and
no-replace behavior. Final log
`/private/tmp/deppy-ssh-musl-build-serial-20261022.log`.

Authoritative coherent artifacts:
`target/remote-host/0.13.0/manifest.json`.
- linux-aarch64:8569664bytes,
  SHA256e71c0fc37b718c2c5fdcc756e03cd93285f897bea6fc064fd06f833dbdda4f5a.
- linux-x86_64:9040424bytes,
  SHA2567673849a4475505f3d6178e5db2e2ba3b5a8f50f38cdfc0ecca02bf3c12ae634.

Actual post-build verification:
`python3 scripts/prepare-remote-host.py --source target/remote-host/0.13.0 --version 0.13.0`

passes static ELF/architecture/no dynamic dependencies/manifest sizes+digests.
Actual11Python packaging/validator/deadline tests PASS again; shell syntax and
`git diff --check` pass. Current artifacts are investigation/test builds under
source0.13.0, not a user release. No macOS package/delivery/App launch was done.
Parent final E2E should use
`DEPPY_SSH_HELPER_ARTIFACTS="$PWD/target/remote-host/0.13.0"`;
future packaging uses
`DEPPY_REMOTE_HOST_ARTIFACTS="$PWD/target/remote-host/0.13.0"` only when release
version/source match. The consent-fixture pair is superseded for final gates.

Parent approved coordinated default exposure only after all final gates:
app Cargo `default=["ssh-workspaces"]` and no-argument package selecting SSH1,
keeping `--features ssh-workspaces` compatibility and explicit Cargo feature
selection. No default mutation has occurred yet. Readonly future package/test
patch is `/private/tmp/deppy-ssh-default-package-prepared-20261022.patch`.
After root release, apply changed packaging test expectation first, run actual
RED with current defaultoff, then change package default and rerun11tests.
Root owns Cargo/default and full feature/defaultoff App gates. Until that source
change is validated, the plan's PR6 flag-removal checkbox remains incomplete.

Readonly remote change-observation review after final pair verification:
current remote ReplaceWatchSet returns WatchUnavailable, so approved PR4 change
observation is still a real integration gap. Existing leaf limits are watch
plan256directories/4MiB paths, capacity1 maintenance, listing4096items/4MiB,
page256; App RPC cap8. Keep timing/admission in App (leaf architecture test
forbids poll_/request_repaint_after); use monotonic completion-relative delay,
one logical listing including pages in flight, no catch-up burst, and reserve
capacity for explicit documents/Git. Stop scheduling on inactive/warm, disabled,
Notes, collapsed, disconnected, closing, or retired attachment/root. TreeSome
alone does not indicate visibility: collapsed/selected_tool are private leaf
state and need a pure visibility accessor. Root DirtyDirectory refresh starts
listing_walk across expanded descendants, so root-per-interval is not itself a
per-tick work bound. Parent/App owner received these constraints and suggested
Instant-driven behavior tests. No product source was changed for this review.

Re-executed exact sealed pair validator, exit0:
`python3 scripts/prepare-remote-host.py --source target/remote-host/0.13.0 --version 0.13.0`.
Readonly git diffs show no helper/runtime/storage/secret/core/rootCargo changes;
Cargo.lock only adds App dependency registrations. App-only observation changes
therefore leave final helper binaries and their prior actual baseline/archive
proof intact. A later canonical release version change requires matching rebuilt
helpers. Package/default exposure remains held pending root's complete gates.

Final Host/resume/bootstrap/package source audit (read-only Rust; no App Cargo):
no additional material Host/resume/bootstrap issue was found in this scope.
Verified source guards: Host phase authenticates and checks expected resolved
peer/key before task(stream) can write hello/resume/install bytes; both initial
and post-install typed ResumeExpired use exact-target clear; metadata checks
account/version/Linux/architecture before Connected. Resume files use owned
private directory0700 and regular private file0600 checks, O_NOFOLLOW/openat,
bounded64KiB reads, exact target identity, fsync+atomic rename. Bootstrap checks
manifest/version/digest/size/ELF architecture and current-generation consent.
This is a source audit, not a new Rust test execution claim. Optional /review
skill pipeline could not run because its required checklist.md was absent;
manual audit continued as parent requested, with no skill PASS claim.

Material packaging trust-order finding was reproduced using a harmless actual
Mach-O/codesign fixture: production verifier executed askpass --version before
rejecting an ad-hoc signed bundle for invalid TeamIdentifier. Evidence:
`/private/tmp/deppy-ssh-package-trust-proof-20261022.log`. Parent authorized the
minimal script fix and promoted regression. New
`scripts/tests/test_package_trust.py` initially ran2tests1PASS1FAIL: rejected
bundle's companion marker existed. Actual RED log
`/private/tmp/deppy-ssh-package-trust-red-20261022.log`.

Moved companion version execution after deep bundle integrity and all requested
production trust gates (same-team Developer ID, hardened runtime, timestamp,
staple and Gatekeeper). verify_bundle is also used on archive extraction, so
both paths apply the ordering. Explicit untrusted development verification
still requires both policy opt-ins and validates the companion version.
Actual GREEN: all13packaging/validator/deadline/trust tests PASS,0failed,0skipped,
`/private/tmp/deppy-ssh-package-trust-green-20261022.log`. The actual signed
fixtures execute only a harmless companion, never the app executable or GUI.
No release artifact was built or delivered. Source/default exposure still held.

Exact checks executed after this fix, all exit0:
`python3 -m unittest scripts/tests/test_prepare_remote_host.py scripts/tests/test_ssh_packaging.py scripts/tests/test_remote_host_deadline.py scripts/tests/test_package_trust.py -v`
`sh -n scripts/build-remote-host.sh scripts/package-macos.sh scripts/verify-macos-package.sh`
`python3 -m py_compile scripts/prepare-remote-host.py scripts/run-remote-host-container.py scripts/tests/test_prepare_remote_host.py scripts/tests/test_remote_host_deadline.py scripts/tests/test_ssh_packaging.py scripts/tests/test_package_trust.py`
`git diff --check`
`python3 scripts/prepare-remote-host.py --source target/remote-host/0.13.0 --version 0.13.0`

### Developer build assets (2026-10-23)

`scripts/dev-build.sh [--release]` is a build-only entrypoint. It builds the App,
MCP proxy and SSH askpass companion with `--locked`, stages the matching sealed
Linux helper pair at `target/Resources/remote-host`, and preserves the developer
signing policy. `dev-run.sh` retains profile/App-argument routing and its explicit
launch. No App launch is part of `dev-build.sh`.

The helper source is `DEPPY_REMOTE_HOST_ARTIFACTS`, or
`target/remote-host/$VERSION`. Missing or invalid assets retain the validator's
reason and direct the developer to `scripts/build-remote-host.sh` or the artifact
override; no automatic Docker build/download is added. The dev-only refresh
validates the replacement before publication, rejects symlinks and foreign
existing content, and preserves previous managed resources after validation
failure. Release artifact validation remains immutable.

Actual RED evidence: parent ran both developer argv tests (4 subtest failures)
and all asset regressions before implementation. A later focused usability test
also failed because the missing-assets diagnostic lacked a recovery hint:
`/private/tmp/deppy-ssh-dev-assets-hint-red-20261023.log`.

Actual final GREEN command, exit0, **23 passed / 0 failed / 0 skipped**:

```sh
python3 -m unittest scripts/tests/test_dev_build.py scripts/tests/test_prepare_remote_host.py scripts/tests/test_ssh_packaging.py scripts/tests/test_remote_host_deadline.py scripts/tests/test_package_trust.py -v
```

Log: `/private/tmp/deppy-ssh-dev-build-final-green-20261023.log`. The 10 developer
tests use isolated temporary trees and fake Cargo, checking debug/release argv,
exact resource mapping, missing/mismatched/symlink resources, safe refresh and
preservation of existing resources. `dev-run.sh` was never executed. No real App
or developer companion was executed; the separate package trust tests execute
only their harmless signed companion fixture.

The real final sealed pair was also copied and refreshed twice in an owned
temporary directory with exact manifest sizes and SHA256 checks; actual exit0:
`/private/tmp/deppy-ssh-dev-real-resources-20261023.log`. No worktree target
resources, user bundle or release artifact was changed. Shell syntax, Python
compilation and `git diff --check` passed. Default exposure remains held by the
parent until its final coherent gates complete.

Final source exposure: App Cargo and default packaging now enable SSH together. Actual normal-default App3345PASS0FAIL42ignored, Python23PASS, strict workspace Clippy/fmt/package-gate/boundary/deps all0. No App release or restart occurred. See final integration QA/evidence for exact commands and matching source hashes.
