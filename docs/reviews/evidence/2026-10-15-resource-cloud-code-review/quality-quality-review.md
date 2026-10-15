| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/cloud_agent.rs:705 | 종료 drain이 실패한 stale 조회를 계속 새로 예약함 | DB 장애가 계속되면 실제 App 종료가 끝나지 않음 | 종료 중 새 stale/bootstrap admission을 중단하고 이미 접수한 durable 작업만 drain |
| medium | crates/app/src/cloud_agent/management.rs:573 | 최초 run bootstrap 실패를 접수 성공으로 영구 고정함 | 일시 SQLite lock 뒤에도 저장된 run/result가 네이티브 화면에서 계속 사라짐 | 완료 성공 시 bootstrap 완료 처리, 실패에는 유계 재시도/수동 갱신 제공 |

검토 기준: canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD d8826f0b, source 0.12.1 및 기존 dirty 수정. 품질/오류 처리 네 번째 관점의 read-only 검토. canonical source/docs/Cargo, 앱 프로세스, 계정/provider에 쓰기/접속 없음. 모든 파일을 줄 단위로 완전 감사했다고 주장하지 않는다.

Details

1. 종료 중 실패한 stale 조회 재예약 (confidence 10/10; 로직 reviewer와 중복 확인)

`shutdown`은 `poll_management()` 뒤 `drain_one -> complete_history -> poll_management`를 반복한다(cloud_agent.rs:705-712). `complete_management`는 pending을 먼저 제거하고, `GetRun(Err)`이면 캐시를 바꾸지 않고 반환한다(management.rs:363-368). `SaveRun(Err)`도 durable 변경 없이 끝난다. 다음 `poll_management`의 heartbeat stale 필터(management.rs:576-584)는 그 캐시 run을 다시 찾는다. 중복 admission 검사는 현재 pending에만 적용되므로 이미 소비한 실패 completion은 재예약을 막지 않는다. `history_ready`도 DB read/save 실패에서는 true를 유지한다. 따라서 DB 장애가 지속되는 동안 원래 대기 작업을 drain하는 대신 새 조회가 무한히 생긴다. `stop_connection()`은 management cache를 지우지 않으며 실제 `App` 종료가 이 함수를 부른다(app.rs:34465). fixture는 logic reviewer가 소유하며 이 worker는 중복 제작하지 않았다. Root의 실제 private scratch 실행 로그 `app-shutdown-stale-repro.log`를 확인했다: compile 성공, 잠금 없는 control은 1 PASS(2ms), 보호 회귀는 1 FAIL이었다. EXCLUSIVE lock을 유지한 700ms 동안 실패 completion 6개가 재예약되었고 종료가 끝나지 않았으며, rollback 뒤 총 717ms에 완료됐다. 지속 DB 장애에서 drain 루프가 새 일을 반복 생성한다는 source 경로를 실제로 재현했다.

2. 최초 cloud run bootstrap 일시 실패 후 복구 부재 (confidence 10/10; Root 실제 RED 확인)

`poll_management`는 최초 `ListRuns`의 큐 접수 성공에 `bootstrap_started=true`를 저장한다(management.rs:557-574). 이후 `ListRuns(Err)`은 `run_history_read_failed`만 설정한다(510-513). 이 flag의 reset 경로가 없으며, 네이티브 Work/Runs 화면은 `management.runs` cache만 읽는다(ui/runs.rs:121). `History`는 DELETE journal과 100ms SQLite busy timeout을 사용하므로 다른 연결의 짧은 EXCLUSIVE transaction만으로도 최초 조회 실패에 도달할 수 있다(agent-mcp/history.rs:149-151). lock 해제 뒤 같은 actor가 정상 응답할 수 있어도 새로운 native bootstrap은 예약되지 않는다. Connections의 Retry 버튼은 Action::Start이며 관리 bootstrap을 갱신하지 않는다; 실행 중에는 Start가 busy 조건에 걸리기도 한다. remote client가 명시적으로 get_run/list_runs를 요청하면 해당 응답이 cache를 채울 수 있으나 native 사용자가 이를 요구하지 않으면 보존된 결과가 사라진 상태가 계속된다.

`bootstrap-regression.tests-only.patch`는 실제 일시 DB lock을 사용한다. durable Completed run을 fixture DB에 기록하고 실제 HistoryWorker Open completion을 수신한 뒤, EXCLUSIVE lock 아래서 production `poll_management`가 접수한 `ListRuns`의 실제 Err completion을 확인한다. lock 해제 후 같은 actor의 별도 ListRuns가 저장된 run을 정상 반환함을 확인하여 DB 복구를 qualify한다. 이어 normal `drain_history` 뒤 native cached run count 1을 기대한다. Root의 실제 private scratch 실행 로그 `app-bootstrap-repro.log`를 확인했다: compile 성공, 0 PASS / 1 FAIL, actual cached run count 0 / expected 1, 실행 시간 0.12s였다. 첫 bootstrap 실제 실패와 동일 actor의 잠금 해제 후 정상 조회를 모두 지난 뒤 캐시 복구 assertion에서 실패했다. worker는 `git apply --check`를 실행해 exit 0을 확인했고 Cargo 실행은 Root가 담당했다.

검토한 정상 경로와 관련 회귀 이름

- 영속 claim 완료 전 입력 금지, 원래 runtime/session/pane/grant/auth/lease 재검사, PTY correlated 결과 뒤에만 durable finish. `pr7_claim_completion_is_required_before_input_dispatch`, `pr7_pending_claim_rechecks_revoke_expiry_and_exact_target`, `input_waits_for_correlated_pty_result_and_read_only_oauth_cannot_type`, `pr7_durable_claim_exists_at_effect_and_admission_unknown_is_not_retried`.
- run Save 결과의 durable truth와 response fence 구분, terminal/Unknown lease 철회. `start_run_ack_and_cache_wait_for_durable_completion`, `start_run_lookup_completion_rechecks_revocation_before_saving`, `durable_terminal_save_retires_lease_even_when_response_deadline_expires`, `committed_run_save_with_lost_permission_reports_unknown_before_retry`.
- 원래 run identity의 actor/workspace/session/generation 유지, 동일 run ID의 actor 구분, 취소는 요청만 표시하고 agent ACK 뒤 상태 변경. `same_actor_cannot_get_or_update_run_using_another_pane`, `persisted_run_beyond_cache_page_remains_exactly_accessible`, `operator_cancel_waits_for_durability_and_same_actor_ack`, `management_ui_scoped_answer_navigation_and_widgets_isolate_duplicate_external_ids`, `management_ui_cancel_request_stays_running_until_the_agent_acknowledges`.
- native navigation은 요청 번호로 늦은 completion을 배제하며 App은 원래 pane+runtime 검증 및 superseded focus 취소 뒤 실제 focused 상태를 성공 조건으로 사용. `late_navigation_completion_cannot_overwrite_a_newer_request`, `management_ui_closed_original_navigation_reports_failure_and_keeps_results`, `management_ui_original_navigation_reports_observed_success`, `management_ui_new_operator_navigation_does_not_leave_the_old_request_opening`.
- 문서 lane은 handle 기반 regular-file 검사/유계 읽기, 저장 전 alias+target revision 검증, save conflict/failure/panic에서 dirty 보존과 scope-close 취소. `full_fix_20261014_document_fifo_does_not_block_load_lane`, `full_fix_20261014_document_symlink_save_preserves_loaded_target`, `full_fix_20261014_document_external_edit_during_temp_write_conflicts`, `full_fix_20261014_document_scope_close_guards_each_lifecycle_target`, `full_fix_20261014_document_submitted_close_cancel_conflict_failure_leave_tombstone`.
- core atomic write는 exclusive UUID temp, 기존 permissions, 실패 시 자기 temp만 cleanup; PTY queue는 부분 writer failure를 AdmissionUnknown으로 구분해 자동 재시도를 막고 marker/sentinel은 이전 query owner를 분리. `full_fix_20261014_atomic_write_preserves_preexisting_temp_symlink_target`, `full_fix_20261014_atomic_write_parallel_saves_have_distinct_temporary_objects`, `full_fix_20261014_atomic_write_cleans_only_its_temp_on_publication_failure`, `pr1_partial_writer_failure_is_unknown_never_a_retryable_queue_refusal`, `split_marker_separates_old_and_new_query_owners`, `exit_file_retires_old_owner_without_reader_and_survives_consumption`, `exit_probe_does_not_follow_links_or_block_on_a_fifo`.

위 normal-path 항목은 이 worker가 호출 연결과 현재 소스에서 확인한 invariant이며, 회귀 이름은 현재 test inventory에서 확인했다. 이 worker는 Cargo를 실행하지 않았으며 단독 test PASS로 표현하지 않는다. Root의 기존 cloud 검사 전체 PASS 통지는 받았지만 개별 log/집계 검증은 Root 보고서 소유이다. 실제 GUI/provider/OS sleep/Windows 동작은 검증하지 않았다. 문서 final revision check부터 rename까지 OS CAS가 아닌 간격은 기존 handoff의 명시 한계이며 새 발견으로 반복하지 않는다. 스타일/중복/이름 선호는 보고하지 않는다.

재현 patch와 Root 실행 명령 (private scratch 전용; 실제 RED 결과는 위 로그에서 확인):

```sh
git apply --check /private/tmp/deppy-cloud-review-quality-20261015/bootstrap-regression.tests-only.patch
git apply /private/tmp/deppy-cloud-review-quality-20261015/bootstrap-regression.tests-only.patch
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo quality_review_20261015_bootstrap_recovers_after_transient_sqlite_lock -- --test-threads=1 --nocapture
```
