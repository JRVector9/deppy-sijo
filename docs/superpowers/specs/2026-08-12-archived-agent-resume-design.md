# Archived Agent Resume Design

## Objective

When Deppy restores a previously running agent pane as a read-only archive, the footer must accurately offer one of these outcomes:

- resume the exact prior CLI conversation when a trustworthy native session ID is available;
- resume the most recent conversation in that working directory when the CLI supports only a verified recent-session fallback;
- explain that continuation is unsupported and offer a new run;
- explain that the agent executable is unavailable and do not offer a button that cannot work.

The existing archived scrollback, pane identity, launch model/options, failure preservation, launcher design, workspace rail, and session design remain unchanged.

## Evidence and constraints

`sessions.agent_id` is the authoritative provider identity. The live database contains archived Claude, Codex, Kimi, and Qwen rows even when the auxiliary `agent_sessions` table has no matching binding. `MuxSnapshot::PaneSnapshot.persistent_session_id` already carries the exact `sessions.id`, so no `RuntimeEvent` or remote protocol change is required.

`agent_sessions.session_id` is a CLI-native conversation token. It is trustworthy only when the existing binding pipeline discovered it. Claude and Codex can currently produce such bindings; Kimi currently cannot. Recent-session flags select by working directory and therefore must not be described as exact pane continuation.

Archived `sessions` rows persist command, arguments, cwd, agent ID, and status regexes, but not launch-only environment variables. Claude and Codex quick launches rely on `DEPPY_AGENT_EXECUTABLE`; without it, the Deppy shim can resolve a transient cmux shim and inject hook flags twice. Generated shims must therefore remove both their own directory and transient `$TMPDIR` shim directories before resolving the real executable.

## Architecture

### Storage projection

Add a bounded `ArchivedAgentResumeRow` to the existing Agent State snapshot. Each row contains:

- `persistent_session_id`: the authoritative `sessions.id`;
- `agent_id`: the persisted Deppy agent configuration ID;
- optional `kind` and `session_id` copied from the matching `agent_sessions` binding.

The projection joins `sessions` to the pane and auxiliary binding inside the existing off-thread Agent State transaction. It is requested together with the existing restore/binding projection and has the same 256-row and aggregate snapshot byte ceilings. The application indexes it by `persistent_session_id` and joins it to live runtime sessions through the existing mux snapshot.

No schema migration and no runtime wire change are needed.

### Resume strategy

Replace the empty-vector convention with an explicit pure value:

- `Exact`: verified provider plus trustworthy native conversation token;
- `RecentInCwd`: verified provider continuation syntax without an exact token;
- `Unsupported`: no verified continuation syntax;
- `Unavailable`: the installed-agent detector completed and did not find the required executable.

While installed-agent detection is pending, the footer shows a non-clickable checking state. Availability changes presentation only; exact/recent argument construction remains a pure provider/token decision.

Verified commands:

| Provider | Exact | Recent working directory |
|---|---|---|
| Claude | `--resume <id>` | `-c` |
| Codex | `resume <id>` | `resume --last` |
| Kimi | `--session <id>` | `-c` |
| Qwen Code | `--resume <id>` | `--continue` |

Kimi exact syntax is registered but is used only if a future trustworthy Kimi binding supplies the token. Current Kimi panes therefore use the explicitly labeled recent-directory behavior. Unverified providers remain unsupported.

### Application and UI flow

When the Restore or BindingSync projection completes, App stores the archived metadata and requests the existing lazy installed-agent detector if necessary. App derives a `ResumePresentation` per restored read-only runtime session and publishes it to `WorkspaceUi`.

The footer renders:

- Exact: existing app-restart message plus `이어서 실행`;
- Recent: `이 폴더의 최근 작업을 이어서 실행합니다.` plus `이어서 실행`;
- Unsupported: `이어 실행을 지원하지 않습니다.` plus `새로 실행`;
- Unavailable: `에이전트를 찾을 수 없습니다.` and a disabled `새로 실행` button;
- Checking: `에이전트 설치를 확인하는 중…` and a disabled button.

Click handling re-derives the plan from current mux/storage/detection state. Exact/recent plans send their arguments through the existing `RespawnArchivedAgent`; unsupported sends no extra arguments for a true new run; unavailable/checking dispatch nothing. Cross-workspace attached panes remain non-interactive.

## Failure behavior

- Missing or malformed persistent metadata degrades to unsupported/new run, never guessed continuation.
- A binding kind that disagrees with the persisted built-in agent ID is ignored; the provider's recent fallback is used instead.
- Empty, oversized, or control-containing native session IDs are rejected.
- If archived respawn fails, the existing read-only pane and scrollback remain intact.
- A missing executable never appears as a successful new run.

## Verification

Implementation uses test-first cycles for:

- exact/recent/unsupported strategy construction and token validation;
- bounded storage join with and without an auxiliary binding;
- persistent-ID-to-runtime-session mapping;
- unavailable/checking presentation and click dispatch policy;
- exact, recent, unsupported, and unavailable footer labels;
- shim resolution in a PATH containing a transient cmux shim before the real executable;
- existing archived respawn preservation and missing-binding new-run behavior.

Focused tests are followed by app/runtime/storage checks, strict formatting/diff checks, and the relevant full package tests. No manual claim replaces an automated result.
