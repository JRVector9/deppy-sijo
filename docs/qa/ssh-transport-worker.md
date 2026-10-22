# SSH transport worker verification — 2026-10-22

PR2 implementation in `/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022`.
The root worker owns `docs/CODEX_HANDOFF.md`, workspace manifests/lock and commits.
No Deppy application was launched/stopped/restarted. No release build or app artifact
was delivered. Workspace version remains 0.13.0 for this investigation/test build;
release must independently increase the version and verify all bundle values.

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
