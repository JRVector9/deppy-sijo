# SSH workspace evidence — 2026-10-22

These small records preserve actual local test results for the six-stage SSH
workspace implementation. They contain no private fixture configuration, keys,
resume proofs, account credentials, server transcripts or raw diagnostic streams.
Full transient logs remain at the `/private/tmp` paths recorded in the JSON.

| File | Purpose |
| --- | --- |
| [gate-results.json](gate-results.json) | Exact commands, exits, durations, log hashes and Rust suite result lines; original failed, fixed and default-exposure cohorts stay separate |
| [source-snapshots.json](source-snapshots.json) | Reproducible aggregate hashes of the dirty product/test source inputs, independent of documentation edits |
| [helper-manifest.json](helper-manifest.json) | Exact version, size and SHA256 of both sealed static Linux helpers, verified against the files when recorded |
| [log-excerpts.txt](log-excerpts.txt) | Selected actual behavioral REDs, focused GREENs, Linux/runtime, install, UI and native packaging QA result lines |
| [ui-evidence.json](ui-evidence.json) | Origin/hash and visual scope for two representative offscreen PNGs |
| [Narrow host trust](02-host-trust-narrow.png), [verified ready](08-verified-ready.png) | Unmodified synthetic UI states; two of the sixteen rendered frames retained here |

The first final runner completed with failures. The fixed four-thread runner also
completed: only the workspace command failed, in an agent-detection counter/mutex
sequence caused by a missing shared fixture lock. Targeted compatibility,
development resource and parallel fixture-lock checks now pass; see the linked
[design review](../../2026-10-22-rust-ssh-design-review.md). The corrected full
four-thread cohort and both final real Linux opt-ins passed with matching
before/after source digests. Coordinated default exposure is enabled in source;
all seven separate normal-default gates passed with matching before/after source
digests. Read-only Cargo metadata confirmed the default feature, and the owned
fixture cleanup completed. Product source is committed as `8489d6532ae69ce0bd3ef035e24e5601a5b1ac8e`.
Historical cohorts ran on dirty integration source, so their aggregate source
hashes distinguish tested bytes independently of Git HEAD. No gates remain running.

The source/helper version is `0.13.0`. The native bundle/ZIP check used harmless
Mach-O stubs and the sealed Linux pair, with ad-hoc development signatures. It
verified resources, archive identity and rejection before companion execution; it
did not package or execute the actual App, establish Developer ID/notarization,
or deliver a release. See the [support and release boundary](../../ssh-workspace.md).
