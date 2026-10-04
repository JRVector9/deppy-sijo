# Followup layout correction implementation plan

> Execution: inline in the current session, per explicit user instruction to apply, commit, rebuild and restart.

**Goal:** Apply the supplied screenshot corrections and fix the previous missed session-tab reduction.
**Architecture:** Reuse common popup choice/body/footer and existing typed reservation state. Fix WindowEditor sizing at the bounded text helper, without changing reservation or runtime input semantics.
**Tech stack:** Rust, egui0.35, egui_kittest, macOS signed local bundles.

- [x] Reproduce actual editor height below3rows with an offscreen rendering test; reproduce right-side model selector and target spacing/color geometry through real shapes/widgets. Assert actual terminal layout header27px (old29px).
- [x] Fix WindowEditor through desired_rows calculated from actual13pt font row height and requested height, minimum3rows. Current egui ignores min_size.y, so that cannot enforce vertical size.
- [x] Fleet header: muted target label,8px explicit horizontal spacing, accent target name; right-side common36px model+effort dropdown. Narrow windows stack target and chooser without clipping. Keep existing supported effort choices, current-setting default, pending guards and typed original reservation.
- [x] Reduce TERMINAL_PANE_HEADER_HEIGHT29→27. Prior TOP_BAR_HEIGHT38→36 belongs to overall titlebar and remains36. Review prior notes/centered-window source and artifact provenance for additional omissions.
- [x] Update popup contract and numbered18 HTML inventory plus accepted centered-window prototype. Bump0.7.0→0.7.1 with27 inherited Cargo.lock entries.
- [x] Run required Cargo gate: focused RED then GREEN, full Fleet/text/input/workspace-related tests, connector-ui/i18n, strict affected Clippy, boundary/dependency checks, fmt and diff checks. Inspect final source diff.
- [ ] Commit only scoped source/docs, build Developer ID-signed local0.7.1 app with gate and unique target folder, verify version/signature/archive. Gracefully replace exact0.7.0 process and verify actual native versions/executable plus sustained liveness. Record commands/results/commit in handoff.

Exact commands: python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -q -p deppy-sijo followup_layout -- --test-threads=1; git diff --check.
