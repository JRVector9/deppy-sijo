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

## Mandatory release version updates

- Every release or deployment containing a feature update, bug fix, performance
  improvement, or other product behavior change MUST increase the app version.
  This includes an updated local app build delivered to the user for use.
  Never ship changed product code under the previously shipped version.
- The canonical app version is `[workspace.package].version` in the root
  `Cargo.toml`. Increase it before the release build/package, update affected
  workspace package entries in `Cargo.lock`, and retain inherited workspace
  versions instead of adding separate hard-coded app versions.
- Use a patch increase for fixes and performance improvements, a minor increase
  for new features, and a major increase for intentionally incompatible public
  changes. The new version must be greater than the last shipped version,
  including releases made from another branch or worktree.
- Verify that the built app's reported version and the macOS bundle's
  `CFBundleShortVersionString` and `CFBundleVersion` match the intended release.
  Record the old version, new version, change summary, and source commit in
  release notes or the project handoff, and report the version to the user.
- A product release is not complete until the version increase and artifact
  version verification are complete. If the version is unchanged, do not
  distribute/deploy the artifact or claim that the release is complete.
- Documentation-only edits and investigation/test builds that are not released
  do not require an app version increase. App version updates do not replace
  runtime wire, database schema, or MCP protocol versioning, and do not grant
  permission to launch or restart the app.

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
