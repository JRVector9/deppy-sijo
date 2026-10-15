# Read-only R04 / R06 peer review

Reviewed exact final transport patch SHA256: `019156239bdf1d53f77bacfff07834531b40172f39cd50743bb6d504c29d42de`. All three final source hashes match its manifest. No remaining actionable defect was found in the scoped deadline, stop, socket ownership and ended-read-consent paths.

Evidence read:

- `ws_api.rs:60,80,92`: raw Read, Write and flush check the authentication deadline and stop before/after IO; write/read syscalls use at most min(remaining, WS_TICK). `authenticate` additionally checks whole-frame completion and token parse/comparison completion. Already buffered frames cannot bypass the post-read whole-deadline check.
- `ws_api.rs:306-314`: failed authentication preserves the guard for unauthorized/Close best-effort writes; success clears the guard and restores both original socket timeouts. This closes the draft automatic-Pong/Close write gap identified during review.
- Local primary dependency source inspected: tungstenite 0.30.0 `protocol/mod.rs:449-477` automatic reply flushes during read and `protocol/frame/mod.rs:161-241` partial frame/header/buffer retention across IO errors. The wrapper retains those states rather than reconstructing a WebSocket.
- `lib.rs:403-416,606-607`: the original connection worker returns its WS slot after serve, and final WebSocket/TcpStream ownership remains with that worker. Dashboard/watch/origin guards and their cleanup are unchanged; connection registration remains after successful authentication.
- `permissions.rs:199,216,229`: only an active actor with live target or existing effective read gets an enabled read control. Revocation passes false read and false ended input through the existing exact-target ACL mutation; after revocation, ended read regrant is disabled. Input remains live-only.
- `access.rs:51-115` and `control.rs:73-115`: permission is actor+session+generation qualified, setter verifies the target identity, and false/false revocation does not open shared global read/input ceilings. Other actor/session permissions remain independent.
- Final tests inspected: real valid trickle auth rejects after deadline; normal partial+fragmented auth crosses a tick, delivers automatic Pong and checks read/write timeout restoration; continuously progressing auth observes stop; real queued Pong/Close flush rejects expiry/stop; actual egui clicks revoke ended consent and verify disabled regrant/input plus preserved other actor/session consent.

Executed: scoped `git diff --check` exit 0; final patch/manifest hash verification exit 0. No Cargo, build, app, provider, DB or source mutation was performed by this peer review. Root must execute the behavioral gates; this is no test GREEN claim.
