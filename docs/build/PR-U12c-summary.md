# PR-U12c — Child Process Tree CPU/RSS Aggregation (기구현 확정)

검증일: 2026-07-09. 계획문서 Lane D의 작업 1~6이 이미 구현되어 있음을 코드로 확인했다.

| 계획 항목 | 구현 위치 |
|---|---|
| 1. pty boundary ProcessIdentity API | `crates/pty/src/process_identity.rs` (redacted Debug) |
| 2. spawn result에 identity | `PtySession::process_identity()` (`pty/src/lib.rs`) |
| 3. Session metadata 전파 | `session::Session::process_identity()` |
| 4. session→process tree 매핑 | `runtime/src/resource_monitor.rs::matching_process_rows` — pgroup 매칭 ∪ root pid ppid-재귀 자손 |
| 5. 플랫폼별 집계 | unix `ps -axo pid,ppid,pgid,rss,pcpu` (비-unix 빈 결과 — macOS 주 타깃) |
| 6. activity view 세션 트리 표시 | 워크스페이스 행 자식 합산 + pane 서브행(CPU/RSS·Np, 2026-07-08) |

회귀 테스트: `session_usage_aggregates_process_group` / `session_usage_falls_back_to_pid_descendants` / `session_usage_unions_process_group_and_pid_descendants` + 변화 게이트(idle-silent).

잔여 없음 — 셸 pid 단독 집계가 아니라 자손(claude/codex/node) 합산이 이미 동작한다.
