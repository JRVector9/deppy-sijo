# PR-B00b — RuntimeHost Boundary

## Outcome

The runtime crate now exposes an inert `RuntimeHostFactory`, an object-safe `RuntimeHost`
lifecycle, and an app-supplied `RuntimeSecretResolver`. Constructing the factory starts no worker,
process, network connection, timer, or polling loop. Creating a host retains the existing single
runtime worker and synchronous shutdown semantics; no Tokio or second runtime was introduced.

`RuntimeSecret` owns the existing non-Clone/non-Serialize/redacted-Debug `SecretString`. Secret
resolution borrows that same allocation for redaction registration and then consumes it into the
child environment without a plaintext clone. Resolver failures cross the boundary only as a static
failure class and never expose raw adapter errors or logical/physical keyring coordinates.

## Launch correlation and wire compatibility

- `SpawnAgent.agent_config_id` remains in the unchanged `RuntimeCommand` variant and postcard
  encoding; a golden fixture locks the legacy 16-byte command.
- The final host and remote ingress reject empty, over-128-byte, or NUL-containing correlation IDs
  before queueing. The worker repeats the check as defense in depth and performs no secret resolve
  or process spawn on invalid direct input.
- The additive terminal `AgentSpawnResolved` event follows the existing `AgentSpawned` or agent
  `SpawnFailed` event in the same worker turn, exactly once for every valid correlated launch.
  Invalid legacy/direct input gets only the sanitized legacy failure and never reflects its ID.
- Runtime protocol version 10 rejects version 9 during the shared plaintext/TLS hello predicate,
  before either peer can decode the new event. Existing event discriminants and command bytes are
  unchanged.
- The terminal workspace renderer intentionally ignores correlation events; app-owned launch and
  approval lifecycle state is the sole consumer during the remaining B00b/SB01 integration.

## Verification

- Root socket-independent runtime suite: 83/83 pass (all non-remote tests except one pre-existing
  process-sampler fixture).
- Changed-path focused tests pass for secret allocation transfer, host pre-enqueue rejection,
  command/event postcard goldens, valid success/failure FIFO and exact-once correlation, invalid
  direct input, remote input validation, v9 rejection, and Plain/Delta event codecs.
- `cargo check -p runtime -p deppy-sijo --all-targets` passes after the app workspace leaf gained
  the intentional no-op correlation arm.
- Strict runtime all-target Clippy, package rustfmt, runtime/app scoped diff-check, dependency law,
  and the unchanged 53-exception boundary gate pass.

The root full run executed all 130 runtime tests: 102 passed, while 27 existing remote fixtures
were denied at loopback bind with sandbox `EPERM`. The pre-existing process-resource test observed
a pid-bearing zero sample twice in this environment; the B00b diff does not touch the sampler or
that test. No production skip or workaround was added. Agent evidence includes an earlier 128-test
full pass and an 84-test socket-independent pass.

## Remaining app integration

`app.rs` still owns the sequential production cutover: compose `AppRuntimeSecretResolver` against
the published physical-slot pointer, construct one `InProcessRuntimeHostFactory`, switch workspace
runtimes to `Box<dyn RuntimeHost>`, and use exact `AgentSpawnResolved` IDs for lazy proxy approval
listener leases. Public compatibility constructors remain until that cutover and must not become a
second long-lived production path.
