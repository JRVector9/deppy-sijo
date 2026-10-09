# Agent session status harness — 2026-10-09

## Final result

Source implementation, concrete review corrections and local verification are complete. The maintained status command passed **605 tests / 1 ignored**; final full App **2817 / 38 ignored** and web **378 / 10 ignored**, strict affected all-target Clippy, workspace format and boundary checks passed. Final narrow gpt-6.1-sol/xhigh review concluded **OK**. Exact final evidence: `/private/tmp/deppy-agent-status-closeout-20261009.log` and `/private/tmp/deppy-agent-status-legacy-notification-review-20261009.txt`.

Coverage remains69 provider scenarios/238 replay steps, nine PWA contracts, nine retained-hook/bounded-ingestion regressions and existing lifecycle/display/installer suites. Prior delegated PWA reports were matched against all four PR-unit commits and the2026-10-08 journal; no omitted work record was found. This is source verification, with no release, app restart, live hook installation, private transcript collection or hosted CI/mobile-device run. The limits below remain explicit.

After the source-only verification phase, the user authorized a local rebuild/restart on2026-10-09. Delivered version **0.9.0** replaces0.8.7; native About and both bundle version fields were verified. See the current handoff for exact release/runtime evidence. This delivery does not turn fixture coverage into live-provider/mobile-device coverage.

## Scope and authority

Session state is reduced in `storage::Db::record_agent_attention` and projected by the existing AgentStateWorker. Parent execution, live child lifetimes, questions, human approvals, answered requests, cancellation and proven completion are separate facts. A parent Stop cannot complete a live child. A tool result resolves only its owned request, not unrelated questions. Silence, a saved title, a restored database row or a stop gate alone is not proof of success.

The test corpus is authored, sanitized **contract examples**, not captured private conversations and not a claim of live verification on every CLI version. Each step runs the real bounded JSON reader → normalizer → SQLite reducer → projection, including database reopen and an isolated Grok events journal. Public payload bodies/arguments are not copied into persisted state; input fingerprints and bounded ownership metadata are used.

## Primary-source comparison

| Source | Inspected revision / document | Applied rule |
|---|---|---|
| [cmux](https://github.com/manaflow-ai/cmux/blob/ed6ee7d86e6b512bb46fd39555bf490acd3a7758/docs/agent-hooks.md) | `ed6ee7d86e6b512bb46fd39555bf490acd3a7758`, CLI/cmux.swift, agent-hooks/feed docs | Native identity and reconnect binding; a permission-request hook may precede auto approval, so owned results must release it. |
| [Orca tests](https://github.com/stablyai/orca/tree/37833ce9a02ccd6754dc9b4eb3266d39ddf8a2fd/src/main/agent-hooks) | `37833ce9a02ccd6754dc9b4eb3266d39ddf8a2fd`, Claude child permission, Codex/Cursor normalization, Grok background, hydrate tests | Keep parent, child and permission lifetimes independent; restored state is not confirmed live telemetry. |
| [Orca status-store document](https://github.com/stablyai/orca/blob/main/docs/reference/agent-status-store.md) | main document; raw document at pinned SHA returned404 | Execution host is the authority; consumers must not independently invent terminal success. Document is not attributed to the pinned SHA. |
| [Claude hooks](https://code.claude.com/docs/en/hooks) | Official documentation read2026-10-09 | Child agent_id is unique; parent transcript and child transcript differ; missing starts do not erase a first child question/approval. |
| [Codex runtime](https://github.com/openai/codex/blob/0ada5d8806cdad498230d5b1b2924091e04c8feb/codex-rs/core/src/hook_runtime.rs) | `0ada5d8806cdad498230d5b1b2924091e04c8feb`, hook runtime/events and protocol.rs | Current SessionMeta.session_id is the root thread ID, shared by children; agent_id distinguishes child threads. Child transcript must not overwrite the parent binding. Earlier research assumed child-native mismatch; protocol.rs corrected that assumption. |
| [Kimi hooks](https://github.com/MoonshotAI/kimi-code/blob/5b936697670ed15444bcb4720bd91e6ee776a941/docs/en/customization/hooks.md) | `5b936697670ed15444bcb4720bd91e6ee776a941`, externalHooks services, toolApprovalService, agentLifecycle | Numeric turn_id, PermissionResult, request IDs and root agent_id=main. UserPromptQueued is not a new turn. TOML only supported fields; preserve user entries. |
| [Grok hooks](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/10-hooks.md) | `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`, events/dispatch/end-hooks | camel/snake aliases can coexist; subagentType marks child events. SubagentStop is gate-only, so no parent-owned lifetime is invented. Grok completion requires an actual completed turn_ended journal record. |
| [Cursor hooks](https://cursor.com/docs/hooks) | Official common schema, lifecycle and permission outputs read2026-10-09 | conversation_id/generation_id, completed/aborted/error distinction. Before-tool observation is not human approval. Observer output must return allow/continue for gate schemas even when parsing/DB/script is absent. |

## Replay coverage

Current corpus: **69 named scenarios / 238 steps**, provider counts claude:23, codex:10, kimi:15, cursor:11, grok:10.

| Layer | Cases maintained |
|---|---|
| Provider replay | Turn/tool/result/completion, duplicate completion, reopen, unrelated tool vs question/approval, foreign-native result, unknown and malformed payload, failure/interrupt, session end with late output. |
| Child ownership | Parallel owned child lifetimes outlive parent completion; first approval/question without start; reverse exit clocks; all6 completion arrival orders; resolved-first and duplicate result tombstones; capacity rollback. Raw ID-based compatibility cases are distinguished from providers that currently omit stable IDs. |
| Provider specifics | Kimi numeric turns/root main/queued prompt/permission results; Grok matching dual input aliases, child gate exclusion, journal completed versus cancelled; Cursor prompt/conversation/generation, aborted/error, late response, before/after shell/MCP observation. |
| Bounded ingestion | Duplicate/conflicting metadata, object-order-independent input hashes, large ignored input/output, metadata/byte/depth caps, exact owned permission alias fallback. |
| Hook installation | Kimi observer migration/idempotence/third-party TOML/disable/missing-script success; Codex parent+child hook registration; Cursor schema outputs, third-party config/idempotence/disable/invalid or oversized config preservation. |
| Display | Existing session status detector tests cover progress, transient requests, ANSI/redraw/quiet/process exit; App merger keeps confirmed requests and errors ahead of old completion, and live children ahead of parent completion. |

## Verification command and maintenance

```sh
scripts/verify-agent-status.sh
```

Local serialized checkout gate:

```sh
DEPPY_CARGO_GATE=/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py \
DEPPY_CARGO_OFFLINE=1 scripts/verify-agent-status.sh
```

The GitHub Actions job **Agent status ownership and provider replay** executes the identical script on macOS. Ordinary workspace checks remain. Add a named payload sequence and expected facts before changing an adapter; retain RED evidence and rerun GREEN. Fixture schema rejects unknown keys; inventory rejects duplicate/empty cases. Update pinned contracts after a provider changes its wire schema. A GitHub-hosted job is not claimed passed merely because its local command passes.

## Current contracts and honest limits

- Kimi observer explicitly tags its provider, recognizes root agent_id=main, and rejects child tool ownership that lacks a proven exit. Kimi currently emits name-only SubagentStart/Stop; Cursor subagentStop can omit subagent_id. Registering a start-only lifetime would leave permanent Running, and deriving IDs from agent type/name would conflate parallel work. Those incomplete lifecycle hooks are not installed. Stable-ID payload compatibility is tested, but full live child reconstruction for these versions requires a provider-supplied stable ID or separately proven task/transcript ownership.
- Grok stop gates may be blocked, and child sessions have subagentType. Gate-only child events are excluded. Successful parent completion is verified from bounded journal content, not an idle notification alone. Finite background-task rosters and scheduled crons are distinct; scheduled tasks must not be counted as continuously executing children.
- Unknown/malformed events fail open and do not replace known state. Unsupported or changed provider schemas retain terminal/process fallback. This is not a claim of infallibility for future versions or missing telemetry.
- No live hook installation, agent prompt, app restart, release artifact, mobile-device verification or private transcript collection is performed by this harness task. Source version0.8.7 remains; delivery requires an increased version and artifact verification.
- PWA active-workspace display now reads a bounded App-owned status projection independently of GUI frames. Raw terminal state, explicit user override, process exit and hook facts remain separate. Repository reads release the input/rebind mutex, and workspace/runtime generation and explicit UUID runtime-token checks reject old-owner results; warm/suspended rows retain the existing snapshot-only display policy.

## Executed evidence

- First recording fix: commit27822baf; storage433/proxy89/App2809 PASS,39ignored, strict checks and source review completed (handoff contains exact logs).
- Initial provider RED3PASS/3FAIL and GREEN6/43 scenarios; actual Cursor/Kimi/Grok contract gaps, after correcting an initial harness oracle that queried a foreign native pending ID.
- Modern Kimi numeric RED5PASS/1FAIL; binding/output RED5PASS/3FAIL; Kimi/Cursor installer RED0PASS/2FAIL; Codex registration RED3PASS/1FAIL; Kimi main/Grok gate RED6PASS/2FAIL. Sources corrected only after reproducing these cases.
- Grok isolated journal positive replay first failed because runner used real HOME, then completed journal passed with injected isolated home. Cancelled journal expected idle boundary was corrected to None to match existing unconfirmed-idle semantics; cancellation is not success. This describes the initial implementation phase; final core correction evidence follows.

- Initial complete gate actually passed3382 tests/39ignored, strict checks and maintained script. Independent source review found3 issues; corrected with regression RED/GREEN. A follow-up exact-ownership Kimi cleanup case also reproduced/corrected. Subsequent review corrections and the final clean core review are recorded below.

- Final ID-boundary gate actually exited0: proxy99PASS/1ignored, strict proxy Clippy and fmt (`/private/tmp/deppy-agent-status-id-boundary-green3-20261009.log`). Focused gpt-6.1-sol/xhigh source review CONCLUSION: OK (`/private/tmp/deppy-agent-status-id-boundary-review-20261009.txt`), no reviewer tests claimed. Control/oversized aliases and cancelled-generation numeric alias replay included. Earlier green/green2 failed because an added stale-generation oracle assumed globally unique request resolution was turn-restricted; corrected it to the actual cancelled-generation contract.

## PWA parity correction (separate source unit)

Desktop and PWA use the same pure `SessionStatus::from_attention` precedence: confirmed question/approval, raw error or displayed request, live hook work (including children), proven completion, then each caller's existing fallback. Explicit user overrides and process-exited rows retain their ownership. App seeds include detected status and an independent override; routine title/workspace snapshots never reseed live status.

`WebRemoteRepository::list_agent_status` is storage-neutral. The App implementation owns its concrete DB connection and reads attention-only snapshots; production web code has no concrete storage capability. The neutral projector accepts only canonical numeric IDs with an exact workspace prefix, bounded input and union size4096, and no prompt/tool body. The worker queries outside the dashboard mutex and checks the captured runtime generation, workspace and concrete runtime token before applying results. The DB read filters attention ownership inside the same transaction as projected facts; a fresh query cannot attach legacy or previous-runtime facts to reused numeric IDs.

Actual initial PWA RED0PASS/2FAIL (`/private/tmp/deppy-agent-status-pwa-red-20261009.log`) became GREEN2, then extended real DB/worker GREEN6 (`/private/tmp/deppy-agent-status-pwa-contracts-green-20261009.log`). Cases exercise GUI-independent parent/child/question/result/exit updates, input and rebind while a repository read is paused, old-workspace result rejection with reused numeric IDs, precedence and bounds. The pre-subscription override, same-workspace generation and fresh-runtime/no-new-hook regressions are included in nine PWA contracts. Full owned product gate actually passed web378/runtime379/App2817 (3574PASS/48ignored) and boundary checks. These are not live mobile-device claims.

### Runtime incarnation evidence

The first PWA source review found a medium: a post-rebind query still attached historical `ws:number` attention to a replacement runtime. A real DB/new-shell/no-new-hook regression reproduced RED0PASS/1FAIL. Each runtime now generates one UUID token, inherited as `DEPPY_RUNTIME_ID` on shell, split/restore, agent and respawn paths. The observer adds it from trusted environment after bounded input parsing; payload metadata cannot supply it. Attention stores an optional backward-compatible `runtime_instance`, and wrong-token results cannot resolve current requests. Warm activation keeps the same token; recreating the worker changes it. PWA filters by the concrete token within the projection transaction. Legacy/missing-token attention is history rather than confirmed current PWA evidence; terminal/process fallback continues until trusted hooks arrive.

Actual ownership gate: PWA9PASS, storage433PASS, proxy99PASS/1ignored. Added storage ownership regression PASS1 and runtime token/budget regressions PASS2; six-package strict Clippy and fmt PASS. Test setup initially referenced nonexistent fixture helpers and failed to compile twice; corrected before GREEN3. The full owned web/runtime/App gate exited0 with3574PASS/48ignored; focused review then identified the retained legacy hook gap below. Final App2817PASS/38ignored and strict storage/runtime/proxy/web/session/App all-target Clippy, fmt and boundary checks also passed before the subsequent proxy-only corrections.

### Legacy hook compatibility

Focused source reviews found retained `needs-input`, `clear` and `turn-done` hooks lost runtime ownership; result-shaped clear did not resolve its legacy request; rejected input became an absent-body clear; and naive namespacing erased anonymous elicitation counting and permission-alias semantics. Each concrete defect was reproduced before correcting it. Owned legacy calls now enter the real reducer and retain bounded notices in the same transaction, bootstrapping native/runtime identity only for trusted current-runtime parent calls. Tokenless callers retain old flag behavior; child, malformed and rejected metadata cannot change the parent.

Request IDs are stable bounded hashes in a separate namespace while preserving reducer semantics: `elicitation:legacy:` for anonymous counts, `tool:legacy:` for permission fingerprints, and `legacy:` for explicit request IDs. Working and resolved tool IDs and groups use the same namespace. Result-shaped clear resolves its own request; Working clear touches only its matching legacy request; a new-turn legacy clear preserves unrelated modern questions and background children. The bounded reader distinguishes genuine empty stdin from rejected input without retaining body contents.

Nine named legacy/rejected-input regressions cover ownership/message, absent-body compatibility, result resolution, malformed ingestion, independent explicit questions, anonymous balance, missing tool start 70 consecutive tool calls while another question stays pending, and parallel legacy notification completion while modern notification/tool facts remain independent. Latest actual RED4PASS/3FAIL became full proxy107PASS/1ignored; strict proxy all-target Clippy/fmt passed (`/private/tmp/deppy-agent-status-legacy-semantics-green-20261009.log`). Before the last notification correction, the maintained gate exited0:604PASS/1ignored (storage426unit+8integration, proxy107, session37, App shim13/merger2, PWA9, runtime2), `/private/tmp/deppy-agent-status-final-maintained-20261009.log`. That review confirmed the semantic fixes and identified the final notification case below. GitHub-hosted CI and live mobile-device validation remain unexecuted.

The subsequent narrow review confirmed the semantic/count/capacity corrections and found a retained permission notification still remained after its tool completed. Actual RED0PASS/1FAIL; fixed stable `legacy:notification:permission` identity and namespace-specific reducer completion. Named regression includes two parallel legacy calls and an independent modern notification/live tool. Targeted GREEN1, strict storage/proxy Clippy and fmt passed. Final narrow gpt-6.1-sol/xhigh source review exited0/CONCLUSION OK (`/private/tmp/deppy-agent-status-legacy-notification-review-20261009.txt`); it executed no tests. The final maintained script actually passed605 tests/1ignored (`/private/tmp/deppy-agent-status-closeout-20261009.log`): storage434, proxy108, session37, App shim13/merger2, PWA9, runtime2. Final affected full App2817PASS/38ignored and web378PASS/10ignored also passed; strict storage/proxy/App/web all-target Clippy, workspace fmt and boundary checks passed. Entire closeout wrapper exited0. Earlier unchanged runtime379 full-suite evidence remains recorded above.
