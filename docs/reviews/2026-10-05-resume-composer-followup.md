# Latest resume and composer follow-up

## Scope and outcome

User deferred immediate restart, asked existing UI work committed/pushed and two additional issues checked. UI0.7.1 source95807b1 and handoff26449130 are pushed. Additional fixes use0.7.2; current0.7.0 PID26830 is kept running. No production agent prompt was sent for QA and user draft/DB/config/transcripts remain intact.

## Confirmed resume defect

The user's Codex session inventory had4195JSONL files across74directories. Scan sorted newest filenames first with4096file cap, but reportedfalse on reaching that cap; wrapper then cleared the entire recent prefix. find_codex_transcript and TranscriptFinder therefore lost the latest native-session source and could retain preceding saved bindings. A4097file synthetic regression reproduced this (RED assertionfalse). The cap now succeeds with exactly the newest4096paths; depth, per-directory4096entries, operation-wide16384entries, read errors and symlink policy remain bounded/fail-closed. It neither guesses another provider/session nor overwrites an exact binding based on a workspace title.

Not every old native ID observed was erroneous: the newer Claude task belongs to a different, subsequently removed pane. Current web pane has its own older exact native ID. Reused runtime hook IDs cannot establish persistent-pane ownership across restarts. We preserved this isolation rather than automatically resuming another pane's newest task. User has not clarified whether “continue” refers to row description or actual agent conversation, so do not claim every reported resume symptom is reproduced.

## Composer evidence and change

Config enablescomposer and Enter send. Bottom field holds a separate per-session draft; it sends viaEnter(default) or↑, with Shift+Enter adding a line. Typing alone does not update the agent's own TUI editor. Native focus observation showed toolbar appearing only while expanded. After blur/restart a nonempty draft could show no send button or delivery feedback.

Now nonempty collapsed drafts retain the existing toolbar, explicit send control, key hint and admission state. Actual offscreen egui regression reproduced missing↑ before change and verifies clicking it stages exactly the same3line Korean prompt, retaining its draft until real admission. Rejected delivery still shows the existing “rejected/draft preserved” state after collapse. Existing originaltarget, durablecheckpoint, IME, draft/dialog preservation and correlated ACK safeguards are unchanged.

Read-only reconstruction of the current saved redacted terminal snapshot: cursor2,28; Codex placeholder classifiedUnknown, choicefalse, bracketedtrue, detectorRunning, draftfalse. This did not reproduce a blocking guard. A test-owned /bin/cat PTY with native-style dim Codex placeholder verifies real admission,3line Korean body+separateCR and returned text from PTY reader. It does not prove the user's real CLI accepted their prompt; we did not send it. Official placeholder styling was compared with [Codex chat composer source](https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/chat_composer.rs).

## Validation

- Scan RED: /tmp/deppy-resume-scan-red-20261005.log; GREEN2passed /tmp/deppy-resume-scan-green-20261005.log.
- Composer RED: /tmp/deppy-composer-affordance-red-20261005.log (correct fixture then missingbutton); focusedGREEN1UI+1realPTY /tmp/deppy-resume-composer-focused-20261005.log.
- Temporary local snapshot probe1passed, removed from source. /tmp/deppy-composer-snapshot-probe-20261005.log outputs metadata only.
- Initial ownedPTY fixture used RustNUL instead of printf's octal escapes and failedspawn; changed to raw string. Unrelated test string accidentally converted by broad replacement was reverted. No liveuser process affected.
- Final affected gates exit0: App/runtime/Connector/i18n +Appintegration3156passed,0failed,34ignored; strictaffectedalltargetClippy-Dwarnings, UIboundary,27crate dependencycheck, fmt andgitdiffcheck pass. /tmp/deppy-resume-composer-final-gates-20261005.log. Finalsource self-review checked retainedscanbounds/errorpolicy, same-targetdraft/ACK semantics and nonemptycollapsedfailurefeedback; no independentCLIreview was performed. Freshrelease/package exit0; source digest unchanged, bothbundleversions/embeddedcompiledversion0.7.2, strictDeveloperID signatures/architecture/archive passed. /tmp/deppy-rebuild-0.7.2-20261005-proof.json.

## Delivery

0.7.1unlaunchedUIbuild→0.7.2additionalfixes; lastshipped0.7.0. Workspaceversion and exactly27inherited lock versions updated, no dependency changes. Sourcecommit 2bf3b9762eb8a201f1d2b3156e7cf6df7cbc3465 pushed onfeat/audit-nine-pr-v0.6.0-20261004; binarySHA256 3c294a59aa01f7e3bcc3a5b9bbcca8a7e5535c60cee0272510b488eba96ccb5c. SourceSHA256 14ddc054e36c4203ab0d98a97ad3c42d563abeb811f123d6eab49f54641e4207. Artifact target/restart-0.7.2-20261005/Deppy Sijo.app andZIP. Localdevelopment package policy, no newnotarization/publicdeployment. Restart deferred; any final artifact must include these latest product changes, not the earlier0.7.1bundle.
