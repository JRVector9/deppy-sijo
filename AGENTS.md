# Deppy Sijo agent instructions

## App restart permission

- Never launch or relaunch Deppy as an automatic part of a code change, build,
  test, or UI review.
- Only stop the running app and launch a new one when the user explicitly asks
  for a restart in the current task. A previous one-time restart request does
  not authorize later tasks.
- A request to rebuild or to inspect the UI does not authorize a restart.
  `scripts/dev-run.sh` launches the app, so do not run it without that explicit
  request. Build without launching when restart permission is absent.

## Terminal diagnostic output

When a final agent response contains two or more independently actionable
diagnostics, review findings, test failures, or risks, it must begin with a
compact Markdown table that is legible in a monospace terminal.

Use this exact column order:

```text
| Priority | Location | Finding | Impact | Next step |
```

- Put the table before any explanatory prose or repeated raw scan output.
- Emit one deduplicated issue per row; merge observations that have the same
  cause and location.
- Sort `critical`, then `high`, `medium`, `low`.
- Keep each cell to a single concise line. Put evidence, stack traces, and
  implementation detail after the table under `Details` only when needed.
- If no reliable location exists, write `—`; do not invent a path or line.
- Do not force a table for a single simple answer, code snippets, or ordinary
  conversational replies.

This is an agent-output convention. Do not try to rewrite arbitrary shell or
full-screen TUI output inside the terminal renderer.

## Handoff policy

For every long-running coding task, continuously maintain
`docs/CODEX_HANDOFF.md`.

Update it after:

- completing a meaningful task,
- modifying architecture,
- running tests,
- encountering a failed approach,
- changing the remaining plan.

After context compaction, read applicable `AGENTS.md` files,
`docs/CODEX_HANDOFF.md`, `git status`, and `git diff` before continuing.
