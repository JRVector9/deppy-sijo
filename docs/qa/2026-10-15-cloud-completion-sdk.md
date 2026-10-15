The official TypeScript MCP SDK `1.32.1` passed two synthetic transport gates on 2026-10-15. An earlier cohort completed a configured 61-second original call. A later `0.13.0` source cohort passed the quick run with canonical payload assertions. Both runs submitted three operations, completed three, resumed a forcibly disconnected original request with one GET, and made zero wait/get/poll tool calls.

| Recorded call | Earlier cohort | Latest source quick gate |
| --- | ---: | ---: |
| Normal original call | 48 ms | 43 ms |
| Configured delayed call | 61,016 ms; 61,000 ms delay | 775 ms; 750 ms delay |
| Forced original POST disconnect | 1,021 ms | 1,028 ms |

The [earlier 61-second result](evidence/2026-10-15-cloud-completion-sdk/observed/result-long-61s.json) and [latest-source quick result](evidence/2026-10-15-cloud-completion-sdk/observed/result-final-quick.json) both record no transport errors. The long call explicitly increased its request timeout beyond the SDK's 60-second default; the normal comparison call used a 500 ms timeout. The latest-source quick gate did not wait beyond 60 seconds. These demonstrate configured client behavior for this fixture. They do not establish real cloud-host deadline support, native-turn proof compatibility, model consumption, or a model acknowledgement.

The fixture starts a loopback CLI server with an in-memory History. Its synthetic completion command persists a receipt before sending the original reply. It performs no GUI launch, PTY input, real provider request, user-database access, or account connection. The SDK runner checks the original JSON-RPC request ID, stable final event ID, queued admission separately from local completion, one input submission per operation, and the original session/cursor on resume.

The [earlier build log](evidence/2026-10-15-cloud-completion-sdk/observed/build.log) finished at 19:38:42 +09:00 and reports workspace packages at development version `0.12.3`. The relevant transport cohort includes commit `d21117dc4d1caad9fbfae3e2e2910d7684122430` (bounded replay cache and post-handoff unknown effect). This precedes canonical receipt normalization in `882cd848`. The build log contains no source HEAD or file hashes, and the working tree included concurrent integration work. The commit identifies the relevant cohort; it is not a complete source snapshot of that execution. Exact source hashes at the time of that earlier run were not captured. The earlier 61-second result must not be presented as verification of final source.

The [latest build log](evidence/2026-10-15-cloud-completion-sdk/observed/final-build.log) reports `agent-mcp 0.13.0`, finished successfully in 23.20 seconds, and has [empty runtime stderr](evidence/2026-10-15-cloud-completion-sdk/observed/final-stderr.log). Root recorded HEAD `9ed6c277a53ea51df228744cd6efec7ebeb2466f`, whose product source is `f9d7c8a7`. The quick runner additionally asserted canonical queued `effect_state`, `retry=false`, and `retryable=false`. [Source provenance](evidence/2026-10-15-cloud-completion-sdk/observed/final-source.json) and a [Rust/Cargo hash manifest](evidence/2026-10-15-cloud-completion-sdk/observed/final-source-SHA256SUMS) were captured after that run while the canonical checkout was clean at the same HEAD and root confirmed no intervening source changes. These hashes cover the selected tracked Rust/Cargo files; they are not a compiled-binary hash or a complete source archive.

The [evidence directory](evidence/2026-10-15-cloud-completion-sdk/) preserves the earlier [SDK runner](evidence/2026-10-15-cloud-completion-sdk/observed/verify-original-long.mjs), [Rust fixture](evidence/2026-10-15-cloud-completion-sdk/fixture-main.rs), original npm manifest and lock, Cargo manifest/lock snapshots, build log, and result. The preserved [Cargo lock snapshot](evidence/2026-10-15-cloud-completion-sdk/observed/fixture-Cargo.lock) lists workspace packages at `0.12.3` and was copied before root regenerated the private fixture lock for the final `0.13.0` build. No Cargo lock digest was captured during the earlier 61-second execution, so exact equality with that run's lock is not independently established. This snapshot is not the final build's `0.13.0` lock. The [portable runner](evidence/2026-10-15-cloud-completion-sdk/verify.mjs) copies the later private runner, adding assertions that canonical `effect_state` stays `queued` and both `retry` and `retryable` stay false. Those extra assertions were not part of the observed 61-second execution. [Provenance](evidence/2026-10-15-cloud-completion-sdk/provenance.json) distinguishes the files. [SHA256SUMS](evidence/2026-10-15-cloud-completion-sdk/SHA256SUMS) checks stored artifacts; these are evidence-file hashes recorded when copied, not retroactive run-time source hashes.

The portable npm manifest pins SDK `1.32.1` exactly. Its lock retains every observed dependency version, resolved URL, and integrity entry. Only root package metadata and the root SDK requirement changed from the observed `^1.32.1` manifest. The SDK tarball integrity is `sha512-2DdE+SJDtzLEEWzY1ZjY7Q+VcPhcV1KisD3zI4u0XZyktsjHum1mwbMI+JaulUBi2OZk+KJAi2uPXzxichPkdw==`. The observed lock hash is `a4325ed00f5511a3a46afab307860a54facbbfd89b3590ec8791f8e4a4cc4c0b`; a portable run will report its own lock hash.

To reproduce from a selected checkout, use Node 20 or newer, Python 3, and the repository's Rust toolchain. Run the following from that checkout. It prepares a separate temporary Cargo package, uses absolute paths for the repository dependency and both vendor patches, and seeds its lock from the selected repository's current lock. No workspace manifest is edited. The first external build may prune that copied lock and add the fixture package; do not add `--locked` to that first build. An earlier unseeded offline build could not resolve a yanked transitive `wnaf` version, which is why copying the repository lock matters.

```sh
REPO="$(git rev-parse --show-toplevel)"
SDK_REPRO="$(mktemp -d "${TMPDIR:-/tmp}/deppy-completion-sdk.XXXXXX")"
export REPO SDK_REPRO
python3 - <<'PY'
from pathlib import Path
import hashlib, json, os, shutil, subprocess

repo = Path(os.environ['REPO']).resolve()
out = Path(os.environ['SDK_REPRO']).resolve()
evidence = repo / 'docs/qa/evidence/2026-10-15-cloud-completion-sdk'
for name in ('verify.mjs', 'package.json', 'package-lock.json'):
    shutil.copyfile(evidence / name, out / name)
(out / 'fixture/src').mkdir(parents=True)
shutil.copyfile(evidence / 'fixture-main.rs', out / 'fixture/src/main.rs')
shutil.copyfile(repo / 'Cargo.lock', out / 'fixture/Cargo.lock')
quoted = lambda path: json.dumps(str(path))
manifest = '\n'.join([
    '[package]', 'name = "deppy-completion-sdk-fixture"',
    'version = "0.0.0"', 'edition = "2024"', '', '[dependencies]',
    'agent-mcp = { path = ' + quoted(repo / 'crates/agent-mcp') + ' }',
    'serde_json = "1"', '', '[patch.crates-io]',
    'winit = { path = ' + quoted(repo / 'third_party/winit-0.30.13') + ' }',
    'alacritty_terminal = { path = ' + quoted(repo / 'third_party/alacritty_terminal-0.26.0') + ' }', '',
])
(out / 'fixture/Cargo.toml').write_text(manifest)
def git(*args):
    return subprocess.check_output(['git', '-C', str(repo), *args])
(out / 'source-head.txt').write_bytes(git('rev-parse', 'HEAD'))
(out / 'source-status.txt').write_bytes(git('status', '--porcelain=v1'))
(out / 'source-diff.patch').write_bytes(git('diff', '--binary', 'HEAD'))
tracked = git('ls-files', '--cached', '--others', '--exclude-standard', '-z', '--', '*.rs', 'Cargo.toml', 'Cargo.lock', '**/Cargo.toml').split(b'\0')
hashes = []
for entry in tracked:
    if entry:
        relative = os.fsdecode(entry)
        path = repo / relative
        if path.is_file():
            hashes.append(hashlib.sha256(path.read_bytes()).hexdigest() + '  ' + relative)
(out / 'source-SHA256SUMS').write_text('\n'.join(hashes) + '\n')
print('Prepared synthetic fixture in ' + str(out))
PY
npm ci --ignore-scripts --prefix "$SDK_REPRO"
CARGO_TARGET_DIR="$SDK_REPRO/target" CARGO_BUILD_JOBS=2 cargo build \
  --offline --manifest-path "$SDK_REPRO/fixture/Cargo.toml"
DEPPY_COMPLETION_FIXTURE="$SDK_REPRO/target/debug/deppy-completion-sdk-fixture" \
  node "$SDK_REPRO/verify.mjs" > "$SDK_REPRO/result-quick.json"
cat "$SDK_REPRO/result-quick.json"
```

`npm ci` downloads the locked npm dependencies; the Cargo command requires its dependencies to be cached for offline use. The unused `winit` patch warning is expected for this smaller fixture graph. On Windows, use the built `.exe` path. The fixture binary is a synthetic server, not the Deppy application.

The quick runner uses a 750 ms delayed call, longer than its configured 500 ms comparison timeout. To reproduce the observed wait beyond 60 seconds, run this separately and retain both the source provenance files and the resulting JSON:

```sh
DEPPY_COMPLETION_FIXTURE="$SDK_REPRO/target/debug/deppy-completion-sdk-fixture" \
DEPPY_COMPLETION_LONG_DELAY_MS=61000 \
  node "$SDK_REPRO/verify.mjs" > "$SDK_REPRO/result-long-61s.json"
cat "$SDK_REPRO/result-long-61s.json"
```

The portable preparation, shell/Python syntax, file checksums, exact fixture copies, and pinned dependency metadata were checked when this document was added. Both private runtime results above were actually executed by the root agent. The committed portable layout was not itself built or executed during documentation preparation.
