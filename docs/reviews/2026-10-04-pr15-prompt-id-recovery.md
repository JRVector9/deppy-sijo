# PR15 — Long prompt IDs and rejected creation recovery

Scope: prompt_library::fresh_id, PromptPaletteAction/creation recovery and App action settlement. Appearance and popup styles are unchanged; existing shared bounded fields are reused.

Independent CLI finding: a 257-character ASCII title fits the 4KiB title limit but created an ID beyond256B; after refusal, the UI restored it as an existing edit so correcting the title never repaired the invalid ID.

Actual RED: `/tmp/deppy-pr15-root-red-20261004.log` exited101,0passed/2failed. Actual generated ID exceeded256B, and an actual egui New Save/refusal retained Some(id). Synthetic UI and temporary data only.

Fix: ASCII base and every collision suffix fit256B. Truncated base owns only its bounded prefix. Upsert carries `creating`; rejected creation restores None while existing edits keep the original identity and valid undo. No existing saved IDs are rewritten. Rejected fields retain exact text. Actual correction/resave regenerates `corrected` and passes host validation.

Actual GREEN: `/tmp/deppy-pr15-root-green-20261004.log` exited0: focused2, full palette13, library29, strict App all-targetClippy -Dwarnings and fmt. Groups overlap. Collision proof creates12 distinct valid IDs from a4096B title. Root diffcheck0. Independent Codex CLI gpt-6.1-sol/xhigh review exited0 with no confirmed remaining bugs in the actual changed source (/tmp/deppy-pr15-cli-result-20261004.txt). Final combined fullworkspace gate follows PR13/14 integration.

Model/permission: all new CLI/agentsgpt-6.1-sol/xhigh. No native Deppy launch/restart, user PTY/files, clipboard, version change or push.
