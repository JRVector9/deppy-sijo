# MCP startup and shared session labels

Execute inline with TDD/workstep. User approved fixing latency then showing session name / AI / reasoning in the existing shared list. User has supplied the display format; no new approval gate. No new GUI restart permission.

## Unit1: slow automatic startup

Evidence: isolated fresh URL at3.367s, actual ready80.267s. System DNS returned cached missing-name errors while certificate-verified direct resolution reached exact MCP metadata200 and Cloudflare HTTPS DNS had correct addresses. Preserve original public reachability check; no system DNS cache flush/global setting edit.

- [x] Failing regression: system DNS failure for generated trycloudflare HTTPS host recovers through bounded fixed Cloudflare DoH; other hosts/proxy keep system resolver; reject mismatched DNS name/private addresses/truncated/non-success reply.
- [x] Add narrowly scoped custom ureq resolver for automatic metadata probe. Default system lookup first; fallback only for resolver failure and generated host. HTTPS DNS1s budget shares the original remaining request deadline; metadata TLS/SNI/host/resource validation unchanged. No new dependency, worker or unbounded queue.
- [x] Actual isolated startup3fresh addresses before/after as needed, real public PTY/own answer roundtrip, full cloud tests; scoped code review/fix.

## Unit2: shared session identification

- [x] Preserve the existing target session title and agent_info_line provider/model/effort metadata. Keep workspace name for disambiguation. Example: workspace · session / Claude · Opus5.5 · high. Shell has localized Shell/effort unavailable; do not invent model or reasoning when unknown.
- [x] Headless UI regression for model+effort present, plain shell fallback with workspace/session disambiguation; existing per-target consent unchanged. Add only metadata field necessary to project actual sidebar info.
- [x] Scoped review/fixes, full cloud and relevant workspace display tests/i18n/format.

## Finish

- [x] Bump0.2.0→0.2.1 for latency/label fixes, lockfile; release build app/proxy/helper; stage signed versioned local bundle without replacing running0.2.0 bundle inode, no GUI launch/restart. Package scripts currently replace target/bundle; stage/build version-specific artifact safely.
- [x] Logical commits, handoff/report/Obsidian journal with actual numbers, failures and limits. No push.

## Actual completion

Source bffcf483 /4ad811af, full cloud29 pass2ignored; actual public roundtrip14.03s; final startup15.467/17.439/11.241s. Older workspace status assertion stillfails at baseline, recorded in final report. Signed0.2.1 local bundle/ZIP verified; no restart. [Report](../../reviews/2026-09-28-mcp-startup-and-session-labels.md).
