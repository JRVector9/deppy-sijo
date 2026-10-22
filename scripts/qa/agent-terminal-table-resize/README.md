# Installed agent table resize harness

Run disposable synthetic conversations through Deppy's real `Session` → PTY →
Alacritty path. This does not launch Deppy, use a user conversation, send a model
prompt, rewrite terminal rows, or change an installed CLI's preferences.

Requires macOS `sandbox-exec`, Python 3.11+, installed native provider binaries,
and the repository's offline Cargo dependencies. Unsupported sandbox/platforms
abort; there is no unsandboxed fallback. Give the provider binary explicitly.
The runner resolves symlinks, records version and binary SHA before/after, creates
one new temporary project/profile for every invocation, and discovers the Rust
test executable from Cargo JSON rather than a hard-coded test filename.

```sh
python3 scripts/qa/agent-terminal-table-resize/run.py \
  --provider codex --binary /absolute/path/to/codex --mode inline --history \
  --target-dir /private/tmp/deppy-table-harness-target
```

Use `--probe /absolute/path/to/already-built-test-executable` to reuse a build.
Supported pairs: `claude classic|fullscreen`, `codex inline|default`,
`grok minimal|fullscreen`. Run each pair both with and without `--history` for
live and old assistant tables. The history fixture adds a separate saved turn
with 40 paragraphs after the table. All runs use 40 rows and 80 → 42 → 120 → 80
columns. Resuming these saved records does not submit the recorded prompts.

Each output directory is printed and retained for review. It contains generated
fixture/config files, raw `output.ansi` (2 MiB cap), bounded screen/history files,
`metadata.tsv`, `probe-status.tsv`, `probe.log`, and `manifest.json`. The manifest
records command, installed version, resolved binary hashes, exact source hashes
(including dirty source), fixture hashes, sandbox check, child status, output
hashes and independent analysis. Timeout/output-cap cleanup tracks test-owned descendants, including detached PTY
processes, and revalidates kernel birth identities before signalling. Child failures propagate unchanged (for example
Rust test exit 101); timeout is 124, output cap 125, analysis failure 1, invalid
experiment 2. Success requires all stages to be valid and responsive.

The child environment is allowlisted; provider credentials, session markers,
proxy settings, SSH agent and inherited hooks are omitted. Only dummy fixture
keys and a loopback discard endpoint are supplied. Real `HOME` is preserved;
provider-specific config paths and XDG config point to the disposable profile.
Sandbox policy denies external IP outbound, keychain service, writes outside the
fixture (except `/dev`), and reads of real provider/config/credential directories.
A resolved executable stored inside `.codex` is the sole read exception. Before
running a provider, a sandboxed socket check must receive EPERM/EACCES. Hooks,
MCP, updates, telemetry and cross-provider compatibility scans are disabled by
fresh configuration/flags/environment; the sandbox remains the enforcement
boundary. No real credentials are copied. Do not point the ignored Rust probe
directly at a user session or use this harness to submit prompts.

Claude fixtures contain only generated user/assistant records and explicitly use
`--permission-mode default`. The test-owned bootstrap may approve the dummy key
and empty fixture directory. For the auto-mode onboarding offer it sends Down,
waits at least 350 ms for the exact selected `No, keep manual mode` label, then
confirms that choice. It never selects auto mode. Unresolved dialogs invalidate
analysis.

## Interpreting results

- Visibility identifies wrapped synthetic row cells. It is independent of resize.
- Border width measures terminal cells, excluding right padding. Korean wide
  characters and combining marks are counted appropriately. Wrapped identifiers
  and statuses are reconstructed per table column, including rule-only Codex
  output, so text in another column cannot split a Korean semantic token.
- Semantics requires all three identifiers/statuses and both Korean phrases.
- Boxed tables require both borders. Rule-only tables require all semantic rows.
  Missing/cropped tables, dialogs and unsupported shapes are `INVALID`, never a
  resize PASS. A reliably measured wrong width can remain a resize FAIL while
  another stage is invalid; the overall exit is still 2.
- Native history is read only for normal screens. Alternate-screen history uses
  the CLI's PageUp/PageDown in the disposable process. The analyzer never falls
  back from missing application history to unrelated native scrollback.

Expected margins are frozen observations of Claude 2.1.305 (4), Codex 0.163.0
(2), Grok 1.0.67 minimal (0)/fullscreen (19). They are not universal layout rules.
Review changes against version and raw evidence when upgrading a CLI. Claude's
42-column card layout is intentionally unsupported by border analysis. Long
alternate-screen tables can exceed one 40-row application viewport; these cases
are invalid until a complete semantic capture is provided. Native history reads
are bounded to the backend's 256-row API limit. The harness does not infer unseen
rows or claim a CLI product failure from its own capture limit.

No result establishes the user's exact Codex case, app pane resize delivery,
remote ownership, wheel routing, selection/copy, Korean draft editing or a product
fix. These require separate tests. The shell characterization test preserves
explicit table/tool stdout; arbitrary terminal cells are never parsed into new
output.

## Deterministic checks

```sh
python3 -m unittest discover -s scripts/qa/agent-terminal-table-resize/tests -v
CARGO_TARGET_DIR=/private/tmp/deppy-table-harness-target cargo test --offline --locked -p session --test terminal_table_resize -- --test-threads=1 --nocapture
CARGO_TARGET_DIR=/private/tmp/deppy-table-harness-target cargo clippy --offline --locked -p session --test terminal_table_resize -- -D warnings
cargo fmt --all --check
git diff --check
```

The normal Rust target contains 2 deterministic tests and 1 ignored installed-CLI
probe. Ignored probe PASS means visibility only; the Python analysis is the
required resize gate. `tests/fixtures/source.json` records the test-owned source
and hashes of frozen real synthetic captures. Their trailing terminal padding is
normalized in the repository; padding itself has a dedicated generated test.
