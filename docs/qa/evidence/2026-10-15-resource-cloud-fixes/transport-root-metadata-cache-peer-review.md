# Read-only metadata / projection cache peer review — 2026-10-15

Result: no new actionable bug, lock cycle, stale-cache path, actor widening, or routing mutation found in the reviewed source. This is source review, not behavioral GREEN or an independent performance measurement.

Reviewed scope:
- CloudAgent AnswerProjection, install_history, cloud_answers and list_sessions response projection.
- RedactionSnapshot, revision mutation coverage, wrapped-redaction helper extraction, RedactionLease release and public export.
- Related authorize_connection/authorize read path, Answer payload/UI usage, and new root regression assertions.

Contracts checked:
- Cache selection holds the corpus mutex from prune/revision check through the returned cached Arc or fully built new projection. Registration/replacement cannot interleave matching fields or make the revision describe a different corpus.
- Cache identity includes the actual immutable source Arc as well as corpus revision. install_history clears the one bounded cache whenever an equal/new accepted history revision replaces the source. No production in-place source mutation path was found.
- Permanent registration, rotating acquisition/refcount changes and grace pruning all call recompute (which advances revision). Rotating lease release explicitly advances revision; first fail-closed transition advances revision. Revision uses checked_add, so it cannot silently wrap and reuse an old cache identity.
- A snapshot prunes expired patterns before exposing its revision; late registration invalidates prior safe cache before the next cloud_answers call. Source answer body and routing fields are retained separately from rebuilt safe display fields. Existing caller-held immutable snapshots remain unchanged as expected.
- Projection redacts body, display connection and run_label from the same corpus. Opaque id/session/run and created values are preserved. Run display label is clipped only after masking.
- list_sessions redacts both title/workspace_name from one snapshot, then clips UTF-8 safely. Target raw metadata and opaque UUID/generation/workspace identities stay unchanged. Existing per-actor read authorization and live-only input flag remain in place.
- No same-service redaction reentry was found under a held snapshot: cloud_answers calls only snapshot matching; list_sessions authorize_connection→authorize/control access does not acquire the redaction mutex. Replacing/dropping cache payloads drops only Arc/String answer data, not RedactionLease callbacks. The RefCell is local to synchronous &self projection and is not held across caller UI work.
- Wrapped matching was extracted without changing empty/oversize/fail-closed/ANSI/wrapped/fragment matching output rules. The new service wrapper acquires the snapshot before empty/oversize checks; this adds a lock in those early-return cases but no incorrect output path was found.

Verification actually performed by this reviewer:
- Read source, baseline diff, mutation/reference searches and regression assertions.
- git diff --check -- the four reviewed files: exit0.
- No Cargo, application, tests, real secrets/accounts/databases, builds or canonical edits were run by this reviewer.

Design tradeoff retained deliberately: rebuilding a changed large projection holds the corpus mutex for the projection work, so registration may wait for that one synchronous rebuild. Cache hits hold it only for prune/revision/Arc reuse. This gives the requested coherent matching and does not itself show a deadlock; fresh-corpus rebuild/GUI/RSS performance remains Root's separate measurement scope.

Root remains responsible for actual affected/App/WS GREEN and integrated verification.

Reviewed file SHA-256 values:
```json
{
  "crates/app/src/cloud_agent.rs": "62d54539a7c5c760b9bf5e8a298d107508611087b0dc9e9e617291729a22b70b",
  "crates/app/src/cloud_agent/bridge_tests.rs": "5faf3f0f87234d3c52f6795343879fd6e9fca9d8be07769203c05366669b65d0",
  "crates/secret/src/redaction.rs": "ab70fa75543e404b14ee97dff214030eae8333f130fe223c39cc8eff9e7d29ec",
  "crates/secret/src/lib.rs": "6a33116e63e1df30180bbd24fd5d4f745cf003f6f2c35b6e882a761c45d1d327"
}
```
