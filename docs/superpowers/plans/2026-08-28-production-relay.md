# Production Deppy Relay Implementation Plan

> **For agentic workers:** Execute one task at a time with test-first evidence. Do not expose a production Relay endpoint until the identity, pairing, E2EE, permission, and revocation gates below are all wired.

**Goal:** Add an opt-in outbound Deppy Relay path that works without Tailscale while preserving the existing Tailscale Serve path unchanged. Both transports converge only after authentication on the existing dashboard/session/viewer behavior.

**Architecture:** Keep the existing loopback `WebRemoteServer`, Tailscale bearer token, Host allowlist, and protocol-v3 socket untouched. Add an independent Relay security and transport boundary. A trusted fixed app origin serves the reviewed mobile shell; a separate untrusted data-plane Relay origin accepts ciphertext only and never serves executable client code. A Mac and a browser each own a device identity; a five-minute one-shot pairing ceremony establishes trust by a user-verified code. Every connected session uses signed ephemeral P-256 ECDH, transcript-bound HKDF-SHA256 directional keys, AES-256-GCM envelopes, monotonic sequence numbers, and explicit per-device permissions. No transport automatically falls back to the other.

**Tech stack:** Rust sync workers, `p256`, `hkdf`, `sha2`, `aes-gcm`, `getrandom`, `serde`, `tungstenite`/rustls for outbound WSS, browser WebCrypto, app-owned persistence adapters, existing `DashboardHandle` projection.

---

## Non-negotiable invariants

- Relay is disabled by default and opt-in independently of `config.web.enabled`.
- Existing `WebConfig`, `pairing::WEB_TOKEN_ID`, `/?token=`, `ws_api`, `tailscale serve`, `ts_hostname`, and Host validation never authenticate Relay traffic.
- Relay URLs are `https://`/`wss://` only; the Mac opens outbound connections only and never binds a new public/LAN listener.
- Pairing tickets expire after five minutes, are single-use, and contain no reusable Tailscale or Relay device credential.
- Device permissions default to view-only. Input, approvals, and uploads are independently enforced on the Mac, not only hidden in browser UI.
- Relay cannot decrypt terminal output, input, approval previews, or uploaded content. Secrets and plaintext never enter logs.
- The untrusted Relay data plane never serves HTML, JavaScript, service workers, or WebAssembly. The trusted shell origin and its release process are explicitly inside the trust model.
- Revocation is checked before channel admission and again at each permission-bearing command boundary.
- Every queue, replay window, pending pairing, device list, reconnect attempt, and ciphertext frame has an explicit resource bound.
- A Relay failure never silently switches to Tailscale, and a Tailscale failure never silently switches to Relay.

First-release capability matrix:

| Principal | Allowed traffic |
|---|---|
| unauthenticated/revoked | no dashboard, session, viewport, or approval data |
| view-only Relay device | dashboard, watch, unwatch, request-keyframe, viewport |
| future input grant | key, input, scroll, switch |
| future upload grant | upload |
| future approval grant | approvals projection, resolve, approval push |

The first production release issues only view-only grants. Input, approval, upload, and permission editing remain dormant future capabilities even though the Mac authorization model must reject them correctly.

## Task 1: Lock the Relay security contract in a standalone module

**Files:**

- Create: `crates/web-remote/src/relay/{mod.rs,contract.rs,pairing.rs,crypto.rs}`
- Modify: `crates/web-remote/src/lib.rs`
- Modify: `crates/web-remote/Cargo.toml`
- Modify: root `Cargo.lock`
- Create: `crates/web-remote/tests/relay_webcrypto_vectors.rs`
- Create: `crates/web-remote/tests/fixtures/relay-webcrypto-v1.json`
- Create: `crates/web-remote/tests/fixtures/relay-webcrypto-v1.js`

### Step 1: Write failing permission and pairing-state tests

Add tests proving:

- `RelayPermissions::default()` grants view only.
- input, approval, and upload checks fail without their corresponding explicit grant;
  granting input alone never grants upload.
- a pairing ticket is accepted only before its five-minute deadline and at most once.
- a confirmation attempt is bounded and a rejected/expired/consumed ticket cannot be revived.
- active tickets plus terminal tombstones have one fixed aggregate bound; clock rollback rejects new
  tickets and terminally expires the targeted existing ticket.
- ticket/debug output elides rendezvous secrets and private key material.

Run the focused tests and retain the RED output before implementation.

### Step 2: Implement bounded pairing state and permission types

Use opaque newtypes for pairing/device/connection identifiers. Keep wall-clock values injectable for deterministic tests. Make invalid transitions explicit errors rather than booleans.

### Step 3: Write failing authenticated-channel tests

Add deterministic tests proving:

- desktop and device derive opposite matching send/receive keys from signed ephemeral handshakes.
- transcript, role, connection id, identity key, or ephemeral key tampering fails authentication.
- ciphertext contains no plaintext fragment.
- wrong-direction, replayed, skipped, duplicated, oversized, and post-close frames fail closed.
- the send counter cannot wrap and plaintext/ciphertext sizes are bounded before allocation.

### Step 4: Implement the browser-compatible channel contract

Use separate long-term ECDSA identity keys and per-connection ephemeral ECDH keys. Bind protocol version, both identity fingerprints, both ephemeral public keys, roles, and connection id into the signature/HKDF transcript. Derive distinct desktop-to-device and device-to-desktop AES-256-GCM keys. Derive nonces from a direction domain plus a monotonic 64-bit sequence; never accept out-of-order input in v1.

The Mac cannot activate a channel from a signed handshake alone. `PairingRegistry` must first consume
the verified one-shot ticket into a non-cloneable approval bound to that exact connection, peer
identity, and transcript hash; `AuthenticatedHandshake::confirm` consumes and checks it. Keep stable
identity fingerprints out of the clear transport header. They remain locally reconstructed AAD while
the clear inner header carries only version, connection id, direction, and sequence.

Do not add sockets, config toggles, persistence, or UI in this task.

### Step 5: Verify Task 1

Before persistence or sockets, run the same fixed identity, ephemeral key, transcript, signature, HKDF, AES-GCM, AAD, sequence, and SAS vectors through Rust and a real browser WebCrypto harness. The Rust unit test reconstructs and compares the checked-in JSON byte-for-byte at the typed-value level. `relay_webcrypto_vectors.rs` is an ignored integration test that embeds that JSON and the fixed JavaScript runner into an isolated local HTML document, launches headless Chrome/Chromium with a bounded temporary profile, and fails unless the browser DOM reports every fixture result as matching. It resolves `CHROME_PATH`, then `CHROME_BIN`, then documented macOS/Linux Chrome or Chromium paths; no browser found is a hard failure, not a skipped or passing result. A mismatch blocks every later task.

Run:

```bash
CARGO_INCREMENTAL=0 cargo test -p web-remote --locked --lib relay:: -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p web-remote --locked --test relay_webcrypto_vectors -- --ignored --nocapture --test-threads=1
CARGO_INCREMENTAL=0 cargo clippy -p web-remote --locked --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Commit only Task 1 files.

## Task 2: Add device identity, pairing, permission, and revocation persistence

**Files:**

- Create: `crates/web-remote/src/relay/repository.rs`
- Modify: `crates/web-remote/src/relay/mod.rs`
- Modify: `crates/web-remote/src/lib.rs`
- Modify: app/storage migration and adapter files selected after schema inspection

### Step 1: Define a storage-neutral repository port

The `web-remote` crate owns bounded records and a `RelayRepository` trait; it must not open SQLite directly. Records include device id, public identity keys, display name, explicit permissions, issued/expires/last-seen timestamps, and revoked-at. They never include Tailscale tokens or raw private keys.

### Step 2: Persist Mac private identity only through `SecretStore`

Use a new versioned Keychain id. Never reuse the VAPID, remote TLS, OAuth, or web pairing key ids. Treat Keychain denial as a Relay-only startup error; do not break Tailscale or app startup.

### Step 3: Add atomic one-shot pairing and revocation operations

Approval must atomically consume the pending ticket and create/update one device. Revocation must invalidate new channel admission immediately. Bound pending tickets and devices, and define deterministic eviction/rejection behavior.

### Step 4: Verify migration, restart, revocation, corruption, and resource bounds

Use repository contract tests plus real SQLite integration. Confirm secrets and terminal payloads never serialize to the DB.

## Task 3: Define and test the untrusted relay data-plane protocol

**Files:**

- Create: `crates/relay-protocol`
- Create: `crates/relay-server` (ciphertext-only binary)
- Modify: root `Cargo.toml` and `Cargo.lock` to register both crates as workspace members
- Create: `deploy/relay/{staging,production}`
- Create: CI/build/publish jobs for the Relay binary and deployment manifests
- Add protocol fixtures shared with `web-remote` without sharing private-key types

### Step 1: Specify the smallest routing envelope

The relay may know protocol version, rendezvous/device routing handle, connection id, sequence, ciphertext length, and liveness timestamps. It must not receive device display names, permissions, session ids, workspace names, terminal contents, input, approval previews, or upload filenames in plaintext. It must not host or proxy the mobile shell.

### Step 2: Implement bounded rendezvous and forwarding

Require TLS at the deployment edge. Define a separate opaque Mac admission credential and pairing-ticket admission policy before allocating rendezvous/queue resources; apply per-credential and per-IP rate limits without logging secrets. Bound connections per account/device, frame bytes, idle time, pairing attempts, and outbound queues. Apply backpressure by disconnecting a slow peer; never buffer without limit.

### Step 3: Add abuse and confidentiality tests

Cover ticket guessing/rate limits, cross-device routing attempts, duplicate connection ids, oversized frames, slow consumers, disconnect cleanup, and a relay-side plaintext scan of full scenario traffic.

Run:

```bash
CARGO_INCREMENTAL=0 cargo test -p relay-protocol -p relay-server --locked -- --test-threads=1
CARGO_INCREMENTAL=0 cargo test --workspace --locked -- --test-threads=1
CARGO_INCREMENTAL=0 cargo clippy -p relay-protocol -p relay-server --locked --all-targets -- -D warnings
CARGO_INCREMENTAL=0 cargo clippy --workspace --locked --all-targets -- -D warnings
```

## Task 4: Add the Mac outbound Relay client without changing Tailscale

**Files:**

- Create: `crates/web-remote/src/relay_client.rs`
- Modify: `crates/web-remote/src/lib.rs`
- Modify: `crates/web-remote/Cargo.toml` and root `Cargo.lock`
- Modify: `crates/app/src/config.rs`
- Modify: `crates/app/src/app.rs`

### Step 1: Add independent config and lifecycle

First extract the dashboard projection/runtime-command core so Relay-only operation does not require constructing the loopback `WebRemoteServer`. Preserve existing listener ownership and tests. Then add a separate `RelayConfig { enabled }`, default disabled. The production endpoint is a fixed release constant shared with the trusted shell CSP; only tests and non-persistent development builds may override it. Starting/stopping Relay must not start, stop, rotate, or rewrite the Tailscale web server or its token. Validate the exact `wss://` endpoint policy before spawning a worker.

### Step 2: Build one bounded reconnect worker

Use one owner thread, cancellation-aware DNS/connect/read/write deadlines, capped exponential backoff with jitter, and a bounded command queue. Stop and join it on disable/app shutdown. Do not reconnect while disabled or after credential revocation/authentication failure without explicit user action.

Enable `tungstenite`'s rustls client with the existing `handshake` feature plus `rustls-tls-webpki-roots`. Production WSS trusts only the compiled WebPKI root set and normal hostname validation; do not add native-TLS, private-CA, insecure-verifier, or system-root fallback paths.

### Step 3: Adapt decrypted messages to the existing dashboard/session core

Reuse dashboard snapshots and runtime command sinks through a transport-neutral adapter. Enforce device permissions before input, key, scroll, switch, resolve, and upload actions. Never pass a Relay credential into protocol-v3 `auth` or the loopback HTTP router.

### Step 4: Test coexistence

Exercise Tailscale-only, Relay-only, and both-enabled lifecycles. Prove failure/disable/credential rotation in one transport does not mutate the other.

Add Relay adapter integration tests with an authenticated view-only principal, a recording runtime-command/upload sink, and a repository seeded with a pending approval. Assert all of the following:

- dashboard and viewport traffic still reaches the view-only device;
- neither the approval snapshot present at connection time nor a later approvals update is emitted to that device;
- forged input, key, scroll, switch, resolve, and upload messages return permission denied without invoking a runtime command, upload sink, or approval repository mutation;
- granting the dormant input capability in a test still does not authorize upload without the separate upload grant;
- repeated forbidden commands hit a bounded violation counter and close the Relay connection without changing Tailscale connection state.

## Task 5: Add desktop enable, pairing approval, and device revocation UI

**Files:**

- Modify: `crates/app/src/ui/settings.rs`
- Modify: i18n catalogs
- Modify: `crates/app/src/app.rs`

Show Tailscale and Relay as independent flat sections with independent state, error, address, pairing, and off controls. `Both` is derived from two switches, not persisted as a migration enum. Device approval grants fixed view-only access in the first release. Device management shows last-seen and expiry and supports immediate revocation; permission-edit controls do not ship yet.

Relay readiness gates pair generation. Pairing UI shows the one-shot expiry and transcript-derived code, announces countdown only at meaningful boundaries, starts focus on cancel/reject rather than approve, and never exposes the secret or reusable credential.

## Task 6: Build and deploy the trusted fixed-origin view-only mobile client

**Files:**

- Create: `web/relay-shell` separate from both Relay data plane and loopback token-gated assets
- Create: `web/relay-shell/src/relay-crypto.js` as the single production and vector-test WebCrypto implementation
- Create: `deploy/relay-shell/{staging,production}`
- Create: CI/build/publish jobs for immutable, versioned shell artifacts
- Modify: `crates/web-remote/tests/relay_webcrypto_vectors.rs` and `crates/web-remote/tests/fixtures/relay-webcrypto-v1.js` to import the production shell crypto module
- Reuse the shared full-screen viewer behavior through an explicit transport adapter

### Step 1: Store non-exportable device private keys in browser storage

Use WebCrypto and IndexedDB. Never put device credentials in URLs, localStorage, logs, service-worker cache keys, or notification payloads. Pairing URLs contain only bounded one-shot ticket material. The trusted app origin is an explicit security principal. Use a restrictive CSP whose `connect-src` names only the fixed Relay origin; the Relay origin returns no executable content. Pin staging/production DNS and TLS ownership, version artifacts immutably, and test service-worker rollback so an old shell cannot silently weaken a new protocol.

### Step 2: Implement pairing and verification UI

Show expiry, retry bounds, the same transcript-derived numeric code on Mac and phone, explicit approval, and rejection/revocation recovery. Do not show sessions before device authentication completes.

### Step 3: Enforce fixed view-only behavior in both UI and Mac

Render no approval, composer, special-key, scroll-command, workspace-switch, upload, or permission-request controls. Show an explicit view-only status. The Mac independently rejects every non-view command; UI checks are defense in depth only. Future input/approval grant UI is a separate post-release plan.

### Step 4: Reuse full-screen lifecycle and recovery contracts

Keep Back/focus, privacy curtain, reconnect draft retention/no auto-send, canvas coalescing, pinch/pan, and session-end behavior equivalent to the tested Tailscale viewer.

### Step 5: Make the shipped crypto module the browser vector authority

Keep transcript construction, ECDSA/ECDH, HKDF, SAS, nonce/AAD, and AES-GCM in one browser crypto module imported by the production shell. The Rust WebCrypto harness must import that same module rather than a copied test implementation. CI emits a sidecar manifest containing the immutable shell archive SHA-256 digest and the imported crypto asset digest.

## Task 7: Production verification and release gates

- For each staging and production shell release, verify the sidecar manifest and exact shell archive SHA-256 digest, then run the Task 1 Rust ↔ browser WebCrypto fixed vectors by importing the crypto module from that exact artifact. Digest mismatch, a source-tree/test-runner fallback, or a different crypto asset is a hard failure.
- Real untrusted-relay E2E: relay memory/log inspection contains no terminal/input/approval/upload plaintext.
- Network failures: DNS, TLS, proxy, captive portal, relay restart, Mac sleep/wake, phone background/foreground.
- Security: replay, tamper, cross-device, revoked/expired device, permission downgrade during a live channel.
- Resource soak: 24-hour reconnect, slow consumer, repeated pairing expiry, device churn, queue/backoff/thread/heap bounds.
- Physical devices: iOS Safari/PWA and Android Chrome/PWA for pairing, pinch/pan, Back, privacy, and view-only recovery. Confirm input/approval/upload controls and traffic remain absent.
- Deploy both staging and production artifacts, verify DNS/TLS/CSP/Origin admission and independent rollback, and run E2E against each exact release digest.
- Existing Tailscale regression: protocol-v3 and loopback server diff isolation plus the full `web-remote` suite.

## Current execution boundary

Implement Task 1 first. Do not claim that Relay connectivity works until Tasks 2–6 are complete, both trusted-shell and untrusted-data-plane artifacts are deployed, and Task 7 passes against their exact release digests. Task 1 is safe to ship dormant because it opens no socket, changes no config, creates no credential, and cannot affect the existing Tailscale path.
