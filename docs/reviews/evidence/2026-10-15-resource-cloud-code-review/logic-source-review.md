| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/cloud_agent.rs:1358 | 공유 목록의 제목·작업공간 표시 이름이 알려진 비밀을 그대로 반환 | 읽기 권한만 있는 MCP 클라이언트에 비밀 노출 | 표시 필드를 현재 redaction corpus로 마스킹 |
| high | crates/app/src/cloud_agent.rs:705 | 종료 drain에서 실패한 stale 조회를 계속 새로 예약 | 외부 DB read lock 또는 반복 오류 동안 App 종료가 끝나지 않음 | 종료 중 신규 bootstrap/stale 작업 생성 중지 |
| medium | crates/app/src/cloud_agent/ui/permissions.rs:198 | 종료 pane의 읽기 권한 체크박스가 비활성화됨 | 유지되는 읽기 권한을 해당 pane에서 철회 불가 | 현재 승인된 읽기는 종료 후에도 해제 가능하게 함 |

# Cloud architecture/logic source review — 2026-10-15

## 기준과 검증 구분

Canonical `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test0.12.1 및 현재 dirty 수정 기준. AGENTS.md, handoff 첫 phase, 변경 목록과 해당 연결 diff, CloudAgent/App/agent-mcp/history worker/web-remote cloud 경로를 읽었다. 제품·canonical 문서·Cargo manifest/lock·실앱·사용자 DB/credential 변경 없음. Cargo 실행은 Root가 직렬 수행했다. 이 report와 합성 tests-only patch만 private 디렉터리에 작성했다.

**새 보호 회귀 3건은 Root의 isolated exact-source scratch에서 모두 compile 성공 후 실제 RED로 재현됐다. 종료 unlocked control은 PASS다.** 아래 신규 로그3개와 기존 정상성 로그를 직접 읽어 확인했다. 본 worker는 Cargo를 실행하지 않았고 canonical/source 변경도 하지 않았다.

## Details

### L01 — list_sessions의 표시 메타데이터 비밀 마스킹 누락

- 심각도 high, confidence10/10. Root `app-metadata-repro.log`: compile 성공,0PASS/1FAIL. 실제 named read-only actor App boundary 응답의 title이 `Terminal review-known-cloud-secret-access-token-20261015`였고 기대 `Terminal [REDACTED]`에서 실패했다. workspace_name도 같은 raw serialization source 원인이지만 title assertion에서 먼저 실패했으므로 별도 실행 assertion 결과로 세지 않는다.
- 문제 위치 `crates/app/src/cloud_agent.rs:1357–1359`, 최초 raw 표시 문자열 생성 `crates/app/src/app.rs:28323–28327`.
- 호출 경로: terminal/user pane title 및 workspace 표시 이름 → App `Target` projection → 인증된 MCP `list_sessions` → `authorize_connection(...false)`/read grant → raw `t.title`와 `t.workspace_name` JSON. `crates/agent-mcp/src/server.rs:850–855`는 normalized payload를 그대로 JSON 문자열로 직렬화하며 후속 redaction이 없다.
- 재현 조건: known credential이 workspace display name 또는 OSC/user pane title에 포함됨; 독립 named token에 해당 pane 읽기만 승인; `list_sessions` 요청. 현재 화면 출력·클라우드 snapshot·설정 권한 표는 같은 비밀을 가리지만 목록 metadata는 그대로 전달한다. 실제 provider 계정이나 권한 확대가 필요하지 않다.
- `list-sessions-redaction-tests-only.patch` / filter `review_20261015_list_sessions_redacts_known_secret_display_metadata`: 실제 independently authenticated named principal, 합성 known-secret execution/redaction 등록, read=true/input=false consent. session_id/generation 원본 보존과 title/workspace_name masking을 동시에 요구한다.
- 권장: routing identities는 유지하고 표시 문자열을 `redact_wrapped_text` 기반 현재 corpus로 마스킹한 뒤 제한한다. 이 branch를 비롯한 모든 outward display DTO field에 동일 계약을 적용한다. 임의 전체 JSON 마스킹으로 routing IDs를 바꾸지 않는다.

### L02 — shutdown stale-query 실패 재예약 루프

- 심각도 high, confidence10/10. Quality reviewer가 같은 원인을 독립 발견했으므로 하나로 dedup한다. Root `app-shutdown-stale-repro.log`: compile 성공, unlocked control1PASS/protection1FAIL. unlocked shutdown2ms; EXCLUSIVE lock700ms 동안6 completion을 반복 생성했고 종료되지 않았다. rollback 이후 총717ms에 종료했다. fixture는 release와 worker join을 마친 뒤 assertion했다.
- 주 위치 `crates/app/src/cloud_agent.rs:705–714`; 원인 연결 `crates/app/src/cloud_agent/management.rs:366–368`, `:576–605`.
- `App` 종료 경로 `crates/app/src/app.rs:34465` → `cloud_agent.shutdown()` → blocking `history.drain_one()` 뒤마다 `poll_management()`.
- stale cached run의 `GetRun`이 실패하면 pending entry는 소비되지만 cached run은 계속 non-terminal/stale이고 `history_ready`도 true다. 이어진 `poll_management()`가 같은 run의 새 `GetRun`을 큐에 넣는다. 따라서 blocking drain은 outstanding0에 도달하지 못하며 `history.shutdown()`에 도달하지 않는다.
- 실제 도달 조건: 60초 이상 heartbeat가 없는 retained run과 file-backed cloud_history DB에 대한 다른 SQLite 연결의 `BEGIN EXCLUSIVE` read lock. 각 DB 읽기는 원래 busy_timeout100ms에서 실패하지만 종료 경로가 무한히 재예약하므로 timeout 합계가 유한하지 않다. malformed stored run 등 지속 GetRun 오류도 같은 control flow를 만든다. 정상 UI 중에도 반복 background query churn 원인이지만 별도 finding으로 중복 세지 않는다.
- `shutdown-stale-retry-tests-only.patch` / filter `review_20261015_shutdown_`: production HistoryWorker + 합성 file SQLite + 실제 durable stale run + production cache install. 초기 management.pending0/actor retained bytes0/one run을 확인한다. unlocked control과 EXCLUSIVE-lock case를 비교한다. 단일100ms read timeout에700ms를 제공하고 완료 횟수를 기록한다. LockGuard가 release/Drop rollback하고 assertion 전에 항상 release→join하여 baseline 실패가 test thread를 영구 대기시키지 않는다.
- 권장: shutdown mode에서 신규 bootstrap/stale polling을 중단하고 종료 전 받아들인 durable work와 그 유한 continuation만 배출한다. 외부 I/O 오류로 종료 loop가 새 일을 계속 만들지 않도록 한다. R01 Codex stdin worker shutdown과는 별도 원인이다.

### L03 — exited pane의 읽기 consent를 UI에서 철회할 수 없음

- 심각도 medium, confidence10/10. Root `app-ended-consent-repro.log`: compile 성공,0PASS/1FAIL. controller의 exited target false→true revocation preflight는 통과했고 실제 egui permissions checkbox 클릭 후에도 read consent가 true여서 보호 assertion이 실패했다.
- `crates/app/src/cloud_agent/ui/permissions.rs:197–210`은 read Checkbox와 접근성 상태를 `active && target.live`로 disable한다.
- `CloudAgent::set_targets`는 exited target의 input/lease만 철회하고 read grant/screens를 유지한다. `authorize(...false)` (`cloud_agent.rs:568–582`)는 target.live를 요구하지 않아 해당 클라이언트의 `read_output`/`read_history`/`notify`가 계속 유효하다. 기존 `bridge_tests::ended_session_keeps_read_consent_and_revokes_input_lease`도 이것을 의도된 backend 계약으로 검증한다.
- `set_connection_access(actor,target,false,false)` (`access.rs:67`)는 exited target에도 안전하게 읽기 권한을 철회한다. 하지만 현재 settings read checkbox는 클릭할 수 없다. `ui.rs`, `ui/advanced.rs`, `ui/history.rs`, `ui/connections.rs`를 포함해 다른 per-session read-revoke 동작이 없음을 확인했다. `take_control`/reclaim은 input만 바꾸고, connection revoke는 그 actor의 모든 pane 권한을 철회한다. 전체 server Stop/connection revoke/closepane로 우회할 수 있다는 사실은 좁은 pane별 UI 권한 철회가 없어지는 원인을 해결하지 않는다.
- `exited-read-revocation-tests-only.patch` / filter `review_20261015_owner_can_revoke_read_consent_after_session_exit`: actual CloudAgent UI harness, named actor read-only grant, target.live=false, controller false→true revocation preflight, 실제 Permissions checkbox click, 실제 consent 상태 검증. source-string assertion이 아니다.
- 권장: active principal에 대해 기존 read consent를 해제하는 동작은 exited target에서도 허용한다. 신규 exited read grant 정책과 live-only input enable 정책은 별개로 유지한다.

## 정상 경로와 읽은 실제 결과

아래 결과는 Root 실행 로그를 직접 읽어 확인했다. 본 worker가 Cargo를 별도로 실행했다는 의미가 아니다.

신규 재현 로그는 모두 `/private/tmp/deppy-cloud-review-root-20261015/`에 있으며 `app-metadata-repro.log`0PASS/1FAIL, `app-ended-consent-repro.log`0PASS/1FAIL, `app-shutdown-stale-repro.log`control1PASS/protection1FAIL이다. 기존 선택 실행은 nested subprocess 중복을 제외하면 총642PASS/0FAIL/17ignored이며 신규 의도된 보호 RED3건과 control1PASS를 그 정상 집계에 합산하지 않는다.

- `/private/tmp/deppy-cloud-review-root-20261015/app-cloud.log`: `cargo test --offline --locked -p deppy-sijo --bin deppy-sijo cloud_agent:: -- --test-threads=1`, exit0,161PASS/0FAIL/3ignored.
- `/private/tmp/deppy-cloud-review-root-20261015/cloud-history-worker.log`: 같은 App target의 `cloud_history_worker::`, exit0,6PASS/0FAIL.
- `/private/tmp/deppy-cloud-review-root-20261015/agent-mcp-web.log`: `cargo test --offline --locked -p agent-mcp -p web-remote --no-fail-fast -- --test-threads=1`, exit0. MCP67PASS/1ignored; Web unit388PASS/1ignored; integrations20PASS/12ignored. nested subprocess1PASS를 추가 합산하지 않는다. command metadata는 같은 directory `test-results.json`.

| 흐름 | Source 계약 확인 | 실제 pass한 대표 test |
| --- | --- | --- |
| 로컬 준비와 실제 인증 구분 | loopback Server 시작/host publication/public verification/activity 표시가 별도 | `setup_state_local_listener_does_not_claim_public_or_client_connection`, `management_ui_issued_credential_waits_for_actual_authentication` |
| MCP → 실제 private PTY → 출력 → 자체 답변 | 인증 principal/현재 epoch → App exact Target → durable claim → Runtime admission → PTY result → durable Finish → original notice | `oauth_http_to_real_pty_output_and_own_answer_roundtrip`, `pr9_http_private_pty_unicode_paste_and_own_answer_roundtrip` |
| Named consent, legacy 격리 | global grant ceiling + principal ACL, read-only scope, 단일 writer lease | `granting_only_a_named_connection_does_not_share_with_the_legacy_bearer`, `granting_named_input_does_not_widen_existing_legacy_read_consent`, `explicit_consent_and_one_writer_are_required_for_new_credentials` |
| 이중 입력 차단/원래 owner | scoped operation tombstone, Grant incarnation/input epoch, runtime/workspace/pane/session/persistent ID, agent execution/mode/continuity admission | `same_external_id_has_two_independent_original_session_answers_and_receipts`, `pr7_pending_claim_rechecks_revoke_expiry_and_exact_target`, `queued_cloud_input_is_cancelled_before_pty_admission`, `pr9_original_execution_exit_during_claim_cannot_rebind_to_fallback` |
| 입력 수신증과 완료 구분 | correlated PTY InputAdmitted만 queued; unknown 자동 재시도 금지; run phase는 별도 actor-reported | `input_waits_for_correlated_pty_result_and_read_only_oauth_cannot_type`, `pr7_durable_claim_exists_at_effect_and_admission_unknown_is_not_retried`, `queued_receipt_cannot_complete_a_run_and_terminal_runs_reject_late_reports` |
| 답변 durable/중복/원래 연결 | notify는 stdin에 쓰지 않음, committed Finish만 notice, actor operation key, 이전 list revision 거부, runtime 변경 시 notice 재바인딩 없음 | `pr7_locked_finish_does_not_block_or_publish_uncommitted_answer`, `pr7_older_list_cannot_erase_new_answer_and_new_runtime_does_not_retarget_notice`, `pr9_notify_own_answer_stays_original_after_switch_and_connection_stop` |
| 상태/취소 관리 | mutation key 직렬화, SaveRun successor revision 검증, actor/target fence 재검증, terminal/Unknown lease retire, cancel request와 same-actor ack 구분 | `start_run_ack_and_cache_wait_for_durable_completion`, `same_actor_cannot_get_or_update_run_using_another_pane`, `operator_cancel_waits_for_durability_and_same_actor_ack`, `heartbeat_loss_persists_unknown_and_cannot_be_reported_completed_directly`, `durable_terminal_save_retires_lease_even_when_response_deadline_expires` |
| History read | exact actor/target cursor namespace,256 cursor/16 outstanding/byte cap, terminal observation만 수행, native offset 불변 | `history_is_correlated_bounded_redacted_and_cursor_cannot_cross_actor`, `next_cursor_reads_older_real_rows_using_actual_page_height`, `history_redacts_known_secret_across_terminal_wraps_before_publication` |
| 서버 종료/회전/복구 일반 경로 | queued permits 즉시 revoke, reconnect opt-in capped backoff, persisted profiles에는 secret/consent 없음, suspend는 input consent 철회 | `pr7_shutdown_cancels_pending_input_claim_and_drains_accepted_answer_finish`, `auth_rotation_retires_old_named_permissions_without_exhausting_capacity`, `profiles_and_mobile_projection_never_restore_consent_or_contain_credentials`, `retry_is_opt_in_capped_and_explicit_stop_cancels_it`, `queued_named_input_after_resume_before_app_frame_is_rejected` |
| PWA operator authority와 reply | transport origin active/epoch/expiry/core lifetime을 App dequeue에서 재검증, observed navigation completion callback이 original socket에만 반환 | `cloud_control_mailbox_rechecks_revocation_epoch_and_permissions_at_drain`, `cloud_control_mailbox_rejects_dropped_origin_and_replaced_core`, `cloud_control_queued_before_direct_server_stop_cannot_execute_on_shared_core`, `cloud_navigation_completion_returns_only_to_requesting_websocket`, `cloud_snapshot_and_controls_require_operator_authentication` |
| local status와 provider identity | known provider/model 문자열은 local execution status의 증거로 사용하지 않음; source 관측 시각 유지 | `local_runtime_status_is_independent_of_model_identity`, `repeated_warm_projection_preserves_original_receive_time` |
| tunnel resource cleanup 정상 | owned child kill/reap 후 reader join, strict generatedhost, bounded read/probe; first exact host publish 이후 probe | `cancellation_reaps_only_owned_child_before_startup_finishes`, `unexpected_exit_is_a_failure_and_timeout_reaps_child`, `first_probe_waits_for_server_host_publication`, `healthy_public_probe_survives_connection_unregister_logs` |

## 범위와 미검증

- 새로운 leak/bottleneck 중 `cloud_answers()` 전체 clone/redaction 매-frame 비용은 performance owner가 별도로 측정하고 있어 이 report에서는 중복 finding으로 세지 않는다.
- R01–R12/P01/C01/C02는 재보고하지 않는다. 현재 Relay deployment BLOCKED, finalcheck→rename gap, connect10초 범위 등 기존 기록은 새 결함으로 세지 않는다.
- `docs/cloud-agent-mcp.md`의 실제 OpenAI dot/Meta Muse/Grok Bot/웹 Custom MCP 계정 호환성은 계속 미검증이다. 이번 private HTTP/OAuth/PTY fixtures는 실제 provider 계정 인증·도구 준수·notify 준수 증거가 아니다. 외부 account 로그인/secret/provider task/public tunnel을 시도하지 않았다.
- 이번 Root 기본 Cargo gate에서 public tunnel/startup measurement3 ignored, real-browser integration12 ignored는 실행 PASS로 세지 않는다. GUI/PWA 실제 기기/OS sleep/Windows 전체 실행을 보증하지 않는다.
- 권한·state·queue는 대체로 유계이며 위3 source 문제 외 새 unbounded retained App heap leak은 확인하지 않았다. 전체 모든 소스 행/OS/provider 완전 검증 주장은 하지 않는다.

## 실행 확정과 후속 범위

Root가 isolated exact-source scratch에 private tests-only patch를 적용하고 직렬 Cargo 실행을 완료했다. compile/cache 오류가 아니라 제품 보호 assertion의 실제 RED3건과 unlocked controlPASS1건을 확인했다. 최신 로그·문서 집계·manifest만 갱신했으며 추가 제품 코드/Cargo 작업은 하지 않았다. Root final report/handoff에 이 실행 증거를 반영한다. 구현/패키징/restart는 이번 read-only review에 포함하지 않는다.
