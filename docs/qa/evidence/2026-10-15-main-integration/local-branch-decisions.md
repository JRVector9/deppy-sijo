# Local branch별 통합 결정

| Branch | Main전용 | Branch전용 | 결정 |
|---|---:|---:|---|
| build/macos-notarization-main | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| build/ureq3-migration-20260907 | 316 | 4 | 대체 이관 완료: #155 HTTP移行; 옛 이력 재merge 제외 |
| codex/grok-catalog | 387 | 5 | 중복 제외: main Grok catalog/usage/status 이관과 이후 상태 harness 유지 |
| codex/grok-lifecycle-correction | 373 | 1 | 중복 제외: main Grok catalog/usage/status 이관과 이후 상태 harness 유지 |
| codex/grok-status-integration | 378 | 1 | 중복 제외: main Grok catalog/usage/status 이관과 이후 상태 harness 유지 |
| codex/grok-usage-parser | 387 | 5 | 중복 제외: main Grok catalog/usage/status 이관과 이후 상태 harness 유지 |
| codex/unicode-path-correction | 373 | 1 | 중복 제외: 현재 raw OsString 경로/NFC 표시/UTF8 owner fence 유지 |
| design/environment-api-preview | 205 | 1 | 제외: 최신 프로젝트 키/연결 관리 시안에 대체된 초기 문서 |
| dev/relay-local-runner-main | 309 | 0 | 이미 main 조상: 추가 merge 불필요 |
| docs/ready-prs-main-final-20260909 | 236 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/audit-nine-pr-v0.6.0-20261004 | 0 | 23 | 포함: main을 포함하는23커밋과 현재 결함 수정 |
| feat/automatic-mcp-connect | 127 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/blank-tab-new-shell | 169 | 25 | 중복 제외: 최신 세션 추가/우측 분할/controller retry(#195) |
| feat/cloud-agent-mcp | 158 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/cursor-usage-status | 210 | 5 | 대체 이관 완료: #186 Cursor usage; 옛 이력 재merge 제외 |
| feat/document-body | 451 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/document-multi-tab | 446 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/document-surface-design | 444 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/environment-api-modern-view | 202 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/environment-application-status | 225 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/file-tree-create-modal | 169 | 20 | 중복 제외: 현재 공통 file_tree_create popup(#194) |
| feat/file-tree-finder-clipboard | 324 | 13 | 중복 제외: 최신 native gesture/root/선택/팝업 fence(#198) |
| feat/file-tree-multiselect-order-clean | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/file-tree-search | 169 | 16 | 제외: 미채택 재귀 검색/새 UI 실험(#193) |
| feat/file-tree-typeahead | 169 | 8 | 제외: 미채택 별도 keyboard-focus 탐색 실험(#192) |
| feat/fleet-one-list-and-relay-wip | 324 | 11 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| feat/fleet-one-list-main | 314 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/git-panel | 601 | 22 | 중복 제외: main의 Git v2 PR210–215가 대체 |
| feat/grok-session-detection-main | 312 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/history-card-redesign | 510 | 2 | 제외: 최신 채택 이력 UI가 대체, 과거 카드 layout 복원 금지 |
| feat/measured-seven-improvements | 134 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/project-key-management-20261012 | 15 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/pwa-reading-pr4-20261008 | 59 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/relay-app-adapter-main | 313 | 7 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| feat/relay-app-adapter-main-clean | 307 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/relay-completion-20260906 | 324 | 2 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| feat/relay-reconnect-core-main | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/relay-shell-main | 313 | 3 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| feat/relay-shell-main-clean | 311 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/scrollback-live-policy | 306 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/scrollback-policy-contract | 312 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/session-row-dot-first | 677 | 4 | 제외: 최신 상태 harness/sidebar/tab 시안이 대체 |
| feat/workspace-credential-env | 230 | 0 | 이미 main 조상: 추가 merge 불필요 |
| feat/workspace-env-sources | 228 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/agent-tui-input-harness | 116 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/agent-tui-korean-submit-order | 119 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/audit-pr1-prompt-delivery-20261004 | 109 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr10-direct-input-latency-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr10-direct-input-latency-wavefinal-followups-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr11-tree-multiselect-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr11-tree-multiselect-wavefinal-followups-20261004 | 110 | 3 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr12-combined-code-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr13-guard-output-freshness-20261004 | 110 | 3 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr14-followup-revoke-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr15-prompt-id-fix-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr15-prompt-id-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr16-corrected-core-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr17-followup-code-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr18-runtime-test-secrets-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr19-final-corrective-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr2-ai-eligibility-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr2-ai-eligibility-wave2-coherent-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr20-move-layout-volume-fixtures-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr21-final-layout-review-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr3-library-recovery-20261004 | 109 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr4-fleet-integration-20261004 | 110 | 3 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr4-palette-cache-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr4-palette-cache-wave2-coherent-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr4r-fleet-review-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr5-session-drafts-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr6-image-budgets-20261004 | 109 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr7-cloud-history-worker-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr7-cloud-history-worker-wave2-coherent-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr8-batch-wake-logs-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr8r-log-error-cap-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr9-remote-paste-20261004 | 110 | 2 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/audit-pr9r-grant-revoke-20261004 | 110 | 1 | 중복 제외: audit worker/snapshot, #216 통합 및 최신 회귀 수정 유지 |
| fix/cjk-cell-fallback-main | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/cloud-agent-ended-sessions | 110 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/document-confirm-per-tab | 442 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/environment-api-context-integration | 169 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/environment-file-consistency | 232 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/environment-review-followup | 212 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/environment-snapshot-state | 233 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/gui-stack-036-20260907 | 317 | 5 | 대체 이관 완료: #154 GUI0.36/IME; 옛 이력 재merge 제외 |
| fix/keychain-noninteractive-main | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/keychain-startup-lazy | 311 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/korean-ime-20260906 | 324 | 2 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/md-drop-render-isolation | 312 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/preserve-terminal-layout | 324 | 6 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/preserve-terminal-layout-main | 312 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/relay-channel-concurrency-20260906 | 324 | 2 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/resize-applied-generation | 300 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/resize-flicker-20260906 | 324 | 2 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/session-agent-isolation | 324 | 12 | 중복 제외: clean 가지와 동일한 예전 격리 수정(#197) |
| fix/session-agent-isolation-clean | 324 | 12 | 중복 제외: 현재 소유권 binding/trusted prompt/fork(#197) |
| fix/session-name-stable | 495 | 2 | 중복 제외: 최신 세션별 trusted prompt/태그 filtering과 title 복원 |
| fix/settings-copy-session-background | 235 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/ssh-screen-stays-visible | 169 | 7 | 제외: held viewport 정책이 현행 replay/live editor와 다름(#191) |
| fix/terminal-toolbar-icon-size | 209 | 3 | 제외: 최신 pane/topbar 승인 디자인 유지 |
| fix/warp-terminal-resize | 315 | 6 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/window-resize-flicker | 324 | 4 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| fix/workspace-open-intent | 312 | 0 | 이미 main 조상: 추가 merge 불필요 |
| fix/workspace-session-menu-md-drop | 315 | 10 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| integrate/environment-ui-main-20260909 | 223 | 0 | 이미 main 조상: 추가 merge 불필요 |
| integrate/ready-prs-20260909 | 238 | 0 | 이미 main 조상: 추가 merge 불필요 |
| integrate/reviewed-main-20261015 | 0 | 23 | 현재 통합 브랜치 |
| investigate/agent-wait-classification | 203 | 0 | 이미 main 조상: 추가 merge 불필요 |
| main | 207 | 0 | 이미 main 조상: 추가 merge 불필요 |
| perf/dotenv-shell-spawn-main | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| perf/mcp-history-answer-index | 122 | 0 | 이미 main 조상: 추가 merge 불필요 |
| perf/mcp-screen-buffer-reuse | 125 | 0 | 이미 main 조상: 추가 merge 불필요 |
| perf/mcp-tunnel-event-waits | 126 | 0 | 이미 main 조상: 추가 merge 불필요 |
| perf/seven-history-storage | 158 | 2 | 중복 제외: 현재 packed cell/grapheme/가시행 공유/렌더 및 스토리지 구현 유지 |
| perf/seven-render-ime | 158 | 18 | 중복 제외: 현재 packed cell/grapheme/가시행 공유/렌더 및 스토리지 구현 유지 |
| perf/seven-snapshot-model | 158 | 6 | 중복 제외: 현재 packed cell/grapheme/가시행 공유/렌더 및 스토리지 구현 유지 |
| perf/seven-visible-rows | 143 | 1 | 중복 제외: 현재 packed cell/grapheme/가시행 공유/렌더 및 스토리지 구현 유지 |
| perf/terminal-streaming-reflow | 313 | 0 | 이미 main 조상: 추가 merge 불필요 |
| preview/warp-relay-20260907 | 315 | 16 | 제외: 과거 이관/실험 가지; main의 후속 transport/layout/입력/파일 구현 유지 |
| tmp/cherry-glyph | 597 | 1 | 중복 제외: 현재 provider logo 공통 구현 유지 |
