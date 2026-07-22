# PR-CR01 summary

## Scope

Consolidate production app construction and concrete persistence/secret ownership in
`crates/app/src/app.rs`, while converting reusable helpers to bounded ports and plans. This PR
does not add a second production bootstrap path, feature-flagged cutover, runtime command, worker,
timer, poller, or dependency.

## Composition-root changes

- `main.rs` is limited to paths/config/logging/renderer/bootstrap setup. Production database open,
  platform secret initialization, initial-workspace selection, orphan-session reconciliation, and
  session-log GC are owned by `App::bootstrap`.
- The app-local `storage.rs` concrete re-export shim is deleted. Production app code outside
  `app.rs` no longer owns or exposes concrete `storage::Db`, storage rows, or
  `KeyringSecretStore`.
- The boundary gate rejects app-side `Db::open`, concrete `Db` ownership/re-export shims, and
  `KeyringSecretStore` outside the production prefix of `app.rs`. There is no allowlist path.

## Port migrations

- Dotenv synchronization uses one bounded `load_workspace_dotenv_plan` then
  `apply_workspace_dotenv_plan` path through `DotenvRepository`. Secret-bearing plans and drafts
  are non-Clone/non-Serialize with redacted Debug; scans are capped at 256 profiles and 4,096
  variables, and redaction-capacity failure prevents persistence.
- Dashboard and push share one app-injected `WebRemoteRepository`. The web-remote crate no longer
  constructs concrete databases or exposes storage rows, and it does not hold its repository lock
  across network transport.
- Runtime construction/subscription/shutdown remains behind the existing app-owned RuntimeHost
  adapter; the RuntimeCommand wire format is unchanged.

## Resource and security invariants

- App bootstrap is the sole production GUI composition seam for concrete database/keyring
  ownership.
- Secret values do not cross helper contracts as Clone/Serialize values and Debug remains
  redacted.
- No helper-side idle thread, polling loop, network connection, or periodic repaint is introduced.
- Production and legacy construction paths are not operated in parallel.

## Verification

- Dotenv focused tests: 22/22.
- Web-remote full tests: 140/140; repository 2/2; push 23/23; dashboard 20/20.
- Xtask composition-root/current-tree boundary suite: 9/9; strict Clippy passes.
- `check-boundary`: zero allowlist capability.
- `check-deps`: 23 crates, no forbidden edge or cycle.
- Final app all-target check, strict Clippy, rustfmt, and diff-check will be recorded after the three
  current shared-tree lanes freeze; this summary does not count the concurrent mid-edit run.

## Remaining gate

The independent `mcp-proxy` binary has its own composition root. Its sole concrete keyring
construction is now in proxy `main.rs`; `BackendSession` receives an injected secret-store port and
lazy initializer. Startup and auth-revision lookup perform zero keyring work, and exact physical-slot
resolution initializes and reads only on cold credential use. Proxy tests pass 53/53 with one
explicit environment soak ignored; source and lazy-slot exact regressions pass 1/1 each, along with
all-target check, strict Clippy, scoped fmt, and diff-check.
