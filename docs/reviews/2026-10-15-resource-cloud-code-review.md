| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | [cloud_agent.rs:705](../../crates/app/src/cloud_agent.rs:705) | R01 종료 drain이 실패한 stale 조회를 재예약 | DB 오류가 지속되면 앱 종료가 끝나지 않음 | 종료 시 신규 관리 작업 예약 중단 |
| high | [pty/lib.rs:1836](../../crates/pty/src/lib.rs:1836) | R02 일반 전경 작업의 별도 process group 정리 누락 | 세션을 닫아도 HUP 무시 프로세스가 잔존 | 현재 전경 그룹의 신원을 고정해 종료 |
| high | [cloud_agent.rs:1358](../../crates/app/src/cloud_agent.rs:1358) | R03 세션 목록의 표시 메타데이터 마스킹 누락 | 읽기 승인된 MCP 클라이언트에 알려진 비밀 노출 | 제목·작업공간 표시 이름 redaction |
| medium | [ws_api.rs:196](../../crates/web-remote/src/ws_api.rs:196) | R04 direct WS 전체 인증 시한이 프레임 읽기에 적용되지 않음 | 느린 프레임이 슬롯을 오래 점유하고 시한 후 인증됨 | 읽기별 남은 시한 및 완료 시각 검사 |
| medium | [management.rs:557](../../crates/app/src/cloud_agent/management.rs:557) | R05 최초 run 조회 실패 뒤 재시도 없음 | DB 복구 뒤에도 저장된 작업이 네이티브 화면에서 누락 | 성공 완료와 접수 상태 분리·유계 재시도 |
| medium | [permissions.rs:198](../../crates/app/src/cloud_agent/ui/permissions.rs:198) | R06 종료된 세션의 읽기 권한 해제 UI 비활성화 | 유지된 읽기 승인을 세션별로 철회할 수 없음 | 기존 읽기 승인 해제 허용 |
| medium | [cloud_agent.rs:306](../../crates/app/src/cloud_agent.rs:306) | P01 변동 없는 답변도 매 UI update 복제·재마스킹 | 지원 범위의 합성 부하에서 호출당 평균 110.8ms | 이력·비밀 corpus revision을 함께 쓰는 cache |

# 병목·리소스·클라우드 에이전트 재리뷰 — 2026-10-15

## 결론과 기준

새 결함 6건과 실측 병목 후보 1건을 확인했다. 앞선 전체 수정의 R01–R12/P01/C01/C02와 번호가 별개이며, 이미 수정된 문제를 재집계하지 않았다. 기존 클라우드 관련 검사 **642 PASS / 0 FAIL / 17 ignored**와 새 보호 회귀의 기대 실패를 구분한다. 기존 검사 통과만으로 아래 누락된 조건이 정상이라고 판단하지 않는다.

검토 대상은 `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test version **0.12.1**, 직전 수정까지 포함한 현재 dirty source다. 시작/종료 422개 source SHA와 tracked product diff가 동일하다. `/Users/jr/Desktop/projects/deppy-sijo`의 오래된 checkout, 실제 앱·세션·사용자 DB·키·clipboard는 변경하지 않았다. 이번 작업은 리뷰이며 제품 수정·배포·커밋·푸시·재실행은 수행하지 않았다.

GPT-6.1-Sol/xhigh로 성능/리소스, 클라우드 로직, 인증/보안, 오류 처리 관점을 분담했다. Root가 호출부·현재 보호 코드·실험 자격 조건을 확인하고 Cargo를 단독 실행했다. 4개 동시 agent 제한 때문에 네 번째 관점은 슬롯 반환 후 진행했다. 보안 worker는 마지막 turn content filter로 중단되어 Root가 부분 관찰을 소스 및 안전한 private loopback 회귀로 검증했다. 별도 read-only `codex review --uncommitted`는 300초 timeout/exit124로 최종 결과가 없으므로 통과 리뷰로 세지 않았다. gstack `review`의 필수 checklist가 없어 그 워크플로를 완료하지 않았고, `codex-reviewer` 및 원인 재현 절차로 대체했다.

아래 로그·tests-only patches·benchmark provenance는 [evidence README](evidence/2026-10-15-resource-cloud-code-review/README.md)와 [manifest](evidence/2026-10-15-resource-cloud-code-review/manifest.json)에 보존했다. 합성 credential, 격리 SQLite, 직접 소유한 PTY 프로세스, private loopback만 사용했다.

## R01 — 종료 도중 실패한 stale 조회가 새 작업을 계속 생성

`App` 종료 [app.rs:34465](../../crates/app/src/app.rs:34465)에서 `cloud_agent.shutdown()`을 부른다. [cloud_agent.rs:705](../../crates/app/src/cloud_agent.rs:705)의 종료 루프는 completion마다 `poll_management()`를 호출한다. [management.rs:363](../../crates/app/src/cloud_agent/management.rs:363)의 `GetRun(Err)`은 pending을 소비하지만 캐시의 non-terminal/stale run은 유지한다. 다음 polling의 stale 필터가 동일 run의 새 조회를 접수한다. `SaveRun` 실패도 durable/cache 상태가 바뀌지 않는 같은 경로가 있다.

격리 file SQLite에 durable stale run 하나를 저장하고, 초기 pending/retained job이 0임을 확인했다. 실제 EXCLUSIVE lock으로 조회를 실패시킨 결과, 각 SQL read의 100ms busy timeout이 끝나도 종료는 새 작업을 반복했다. lock 700ms 동안 **6 completion**, 종료 미완료; 명시적 rollback 후 **717ms**에 종료했다. unlocked control은 **2ms / 3 completion**으로 종료했다. **1 control PASS / 1 보호 회귀 FAIL**, compile 성공/exit101. 테스트는 assertion 전에 rollback과 thread join을 완료했다.

700ms 관찰 자체가 무한 대기의 전 시간을 측정한 것은 아니다. 그러나 소스에서 완료된 오류마다 동일 조회가 새로 접수되고 종료 flag가 이를 막지 않아, 지속 오류 조건에서는 finite drain이 되지 않는다. 기존 Codex stdin worker 종료 수정과 원인이 다르다.

수정 방향: 종료 시작 때 새 bootstrap/stale admission을 닫고, 이미 접수한 durable 작업 및 필요한 유한 continuation만 drain한다. 정상 실행 중 DB 오류에는 유계 backoff를 적용한다. R05의 bootstrap 재시도와 함께 설계해 종료 중 재시도가 다시 활성화되지 않게 한다.

증거: [shutdown log](evidence/2026-10-15-resource-cloud-code-review/root-app-shutdown-stale-repro.log), [tests-only patch](evidence/2026-10-15-resource-cloud-code-review/logic-shutdown-stale-retry-tests-only.patch).

## R02 — 일반 전경 프로세스가 pane/세션 종료 후 생존

[pty/lib.rs:1836](../../crates/pty/src/lib.rs:1836)의 Drop과 [pty/lib.rs:2023](../../crates/pty/src/lib.rs:2023)의 kill은 frozen workload를 별도로 처리하지만, 일반 job-control 전경 그룹에는 cached shell group 중심으로 신호를 보낸다. 실제 전경 job은 shell과 다른 PGID를 갖는다. master close로 전달되는 HUP을 그 job이 무시하면 뒤의 TERM/KILL이 전경 그룹에 도달하지 않는다. pane close/kill/close-tab의 실제 runtime 경로가 이 Drop/kill을 사용한다.

현재 production PTY rlib를 사용한 실제 macOS PTY 하네스에서 `/bin/bash --noprofile --norc -i`, private cwd, `HISTFILE=/dev/null`로 실행했다. Python fixture는 HUP만 무시하고 TERM에는 정상 종료되며 15초 hard lifetime을 갖는다. PID·birth time·PGID·SID를 고정한 결과 **일반 HUP 무시 job은 Drop 및 kill+Drop 모두 같은 신원으로 생존**했다. HUP 정상 job과 frozen HUP 무시 job은 사라졌다. Root 독립 재실행도 동일했다. 모든 fixture는 신원 재검사 후 cleanup을 완료하고 보호 assertion에서 exit101했다.

이 결함은 별도 workload 프로세스 누수다. reader/writer worker join 누수 또는 모든 background/escaped descendant 누수로 확대하지 않는다. 실제 native close-button GUI E2E는 수행하지 않았다.

수정 방향: master를 닫기 전에 현재 전경 그룹과 소유 session 관계·member birth/PGID/SID를 고정한다. 기존 `workload_signal`의 신원 보호를 재사용해 shell과 소유 전경 그룹에 유계 HUP→TERM→KILL을 적용한다. 재사용된 PID/PGID, 앱 자신의 그룹, 관계없는 프로세스에는 신호를 보내지 않는다.

증거: [독립 PTY log](evidence/2026-10-15-resource-cloud-code-review/root-pty_nonfrozen-independent.log), [하네스](evidence/2026-10-15-resource-cloud-code-review/perf-pty_nonfrozen.rs), [identity](evidence/2026-10-15-resource-cloud-code-review/perf-identity.json).

## R03 — list_sessions 제목·작업공간 이름에 알려진 비밀이 그대로 반환

[app.rs:28324](../../crates/app/src/app.rs:28324)에서 pane/workspace 표시 문자열을 Target에 복사하고, [cloud_agent.rs:1358](../../crates/app/src/cloud_agent.rs:1358)의 `list_sessions`가 이를 raw JSON으로 반환한다. MCP tool response 직렬화에 후속 redaction이 없다. 출력 snapshot/답변/UI의 마스킹 계약에서 목록 metadata만 빠진 경로다.

실제 independently authenticated named principal, read=true/input=false consent, 합성 known-secret redaction lease로 회귀를 실행했다. 제목에 `[REDACTED]`를 기대했지만 합성 credential 원문이 반환됐다. **0 PASS / 1 FAIL**, compile 성공/exit101. 실제 credential은 사용하지 않았다. 해당 원인은 title과 workspace_name 둘 다에 적용되지만 실행 assertion은 title에서 먼저 실패했다. 권한 우회가 아니라 정상 읽기 권한 사용자에게 불필요한 비밀이 전달되는 문제다.

수정 방향: outward 표시 필드 `title`·`workspace_name`을 현재 corpus로 마스킹하고 출력 길이를 제한한다. session UUID/generation/workspace routing identity를 전체 JSON redaction으로 바꾸지 않는다. 늦은 credential 등록/회전 직후에도 display가 다시 마스킹되는 회귀가 필요하다.

증거: [metadata log](evidence/2026-10-15-resource-cloud-code-review/root-app-metadata-repro.log), [tests-only patch](evidence/2026-10-15-resource-cloud-code-review/logic-list-sessions-redaction-tests-only.patch).

## R04 — direct PWA WebSocket의 전체 인증 시한 누락

[ws_api.rs:196](../../crates/web-remote/src/ws_api.rs:196)의 `authenticate()`는 `ws.read()` 호출 전만 전체 5초 시한을 검사한다. TCP read timeout 300ms는 개별 read의 시한이어서, 한 프레임이 여러 번 정상 read로 도착하면 프레임 전체 읽기가 5초를 넘는다. 완성된 유효 auth 메시지에 완료 시각 재검사가 없다. 별도 direct WS 경로이며 앞선 Relay TLS 전체 시한 수정과 별개다.

private loopback에서 파싱 자격을 확인한 정상 `ClientMsg::Auth` 프레임을 천천히 전달했다. 실제 production authenticate/5초 timeout/300ms read tick에서 **6.363초 후 accepted=true**였다. **0 PASS / 1 FAIL**, compile 성공/exit101. socket close/writer join 완료. 최초 fixture는 `type:"auth"`가 빠져 5.871초 read overshoot만 입증했으며, 시한 뒤 유효 인증 수락 근거로 쓰지 않았다. 수정 fixture의 qualified log가 최종 근거다.

서버는 인증 전에 최대 3개 WS slot 중 하나를 예약하므로 긴 partial frame은 슬롯 유지 시간을 늘릴 수 있다. 실제 서비스에서 3개 슬롯을 소진하는 공격/E2E는 실행하지 않았다. 서버 stop의 socket close가 있어서 이 문제를 앱 종료 worker 누수로 보고하지 않는다.

수정 방향: tungstenite 아래 read wrapper에 남은 전체 시한/stop slice를 적용하고 유효 프레임이 완성된 뒤에도 인증 시각을 검사한다. 정상 fragment 진행과 끊김/취소 동작을 유지한다.

증거: [qualified WS log](evidence/2026-10-15-resource-cloud-code-review/root-direct-ws-qualified-repro.log), [tests-only patch](evidence/2026-10-15-resource-cloud-code-review/root-direct-ws-tests-only.patch).

## R05 — 최초 cloud run 조회 일시 실패 후 화면 복구 없음

[management.rs:557](../../crates/app/src/cloud_agent/management.rs:557)는 첫 `ListRuns`가 접수되면 `bootstrap_started=true`를 설정한다. [management.rs:510](../../crates/app/src/cloud_agent/management.rs:510)의 실패 completion은 오류만 기록하고 이 상태를 되돌리거나 재시도하지 않는다. 네이티브 작업 화면은 이 cache를 읽으므로 초기 DB lock이 풀려도 저장된 run이 계속 누락된다. 클라이언트가 별도 조회하면 cache를 채울 수 있지만 정상 UI가 자동 복구되는 것은 아니다.

격리 SQLite에 Completed run을 저장하고 실제 HistoryWorker가 정상 Open한 뒤, EXCLUSIVE lock 아래 첫 `ListRuns`의 실제 실패를 확인했다. rollback 후 같은 worker의 독립 `ListRuns`가 저장된 run 1개를 반환해 DB 복구를 검증했다. 이어 normal bridge drain 이후 네이티브 cache는 **expected1 / actual0**이었다. **0 PASS / 1 FAIL**, compile 성공/exit101, cleanup 완료.

수정 방향: bootstrap in-flight와 성공 initialized 상태를 분리하고, 일시 오류 뒤 유계 backoff/갱신을 제공한다. R01의 종료 admission 차단 아래에서만 재시도해야 한다.

증거: [bootstrap log](evidence/2026-10-15-resource-cloud-code-review/root-app-bootstrap-repro.log), [tests-only patch](evidence/2026-10-15-resource-cloud-code-review/quality-bootstrap-regression.tests-only.patch).

## R06 — 종료 세션의 읽기 권한을 UI에서 철회할 수 없음

[permissions.rs:198](../../crates/app/src/cloud_agent/ui/permissions.rs:198)는 read checkbox를 `active && target.live`에서만 활성화한다. backend는 종료 세션의 read consent와 마지막 화면을 의도적으로 유지하며, `set_connection_access(...false,false)`로 이를 해제할 수 있다. UI만 비활성화해 세션별 철회가 막힌다. actor 전체 revoke/서버 stop은 가능하지만 다른 세션의 승인까지 제거한다.

실제 egui harness에서 named actor read-only 승인을 부여하고 같은 exact target을 종료 상태로 바꿨다. read=true/input=false와 backend 해제 가능성을 먼저 확인한 뒤 실제 설정 read checkbox를 클릭했다. 승인은 **true로 유지**됐다. **0 PASS / 1 FAIL**, compile 성공/exit101. 다른 UI의 per-session read 해제 경로는 찾지 못했다.

수정 방향: active actor의 기존 read consent를 해제하는 동작은 종료 세션에서도 허용한다. live-only input 정책은 유지하고, 종료 세션에 새 read 승인을 부여할지는 별도 정책으로 결정한다.

증거: [consent log](evidence/2026-10-15-resource-cloud-code-review/root-app-ended-consent-repro.log), [tests-only patch](evidence/2026-10-15-resource-cloud-code-review/logic-exited-read-revocation-tests-only.patch).

## P01 — 답변 projection의 반복 redaction 비용

[cloud_agent.rs:306](../../crates/app/src/cloud_agent.rs:306)의 `cloud_answers()`는 모든 답변을 복제하고 message/connection/run label을 다시 마스킹하여 Vec/Arc를 만든다. [app.rs:35371](../../crates/app/src/app.rs:35371)의 매 update에서 호출되며, stopped/hidden 경로 [app.rs:28219](../../crates/app/src/app.rs:28219)도 동일 작업을 한다. 원본 이력과 corpus가 변하지 않아도 비용이 반복된다.

현재 메서드/Answer struct/실제 redaction 모듈을 사용한 `rustc -O` 합성 component를 실행하고 Root가 독립 재실행했다. 100 답변×16KiB, 합성 key32개/64 corpus patterns는 현재 count/body/history/corpus 한도 안이다. Root 10회 평균 **110.782ms**, 호출당 **14,902 allocation / 37,102,106 누적 allocated bytes**였다. worker 평균은104.487ms였다. 아래는 Root 결과다.

| 부하 | 평균 ms/호출 | allocation 횟수 | 누적 allocated bytes/호출 |
| --- | ---: | ---: | ---: |
| 10×1KiB, key32개 | 0.799 | 1,492 | 638,216 |
| 100×16KiB, key1개 | 8.204 | 2,502 | 32,985,306 |
| 100×16KiB, key32개 | 110.782 | 14,902 | 37,102,106 |
| 100×16KiB, key128개 | 436.846 | 53,302 | 49,850,906 |

이는 최적화된 component 측정이며 전체 release 앱/GUI frame/RSS 측정이 아니다. System allocator counter 비용이 포함된다. 누적 allocation은 생성·재할당 트래픽이며 retained heap 누수가 아니다. 별도 debug component 로그는 release 성능 근거로 사용하지 않는다. 안전한 텍스트 동일성·wrapped secret·늦은 등록 직후 redaction·routing identity 유지 계약은 실제 실행에서 PASS했다.

수정 방향: raw answer history revision과 단조 증가하는 redaction corpus generation을 함께 cache key로 사용한다. source와 corpus가 모두 같으면 안전한 projection Arc를 재사용한다. credential 등록/교체/lease 변경 시에는 표시 전에 무효화한다. history만 cache key로 쓰면 이미 수정된 late-secret 보호를 되돌리므로 금지한다. 큰 projection을 worker에 옮기더라도 corpus 변경 뒤 이전 projection을 게시하지 않아야 한다.

증거: [독립 optimized log](evidence/2026-10-15-resource-cloud-code-review/root-cloud_answers_optimized-independent.log), [component source](evidence/2026-10-15-resource-cloud-code-review/perf-cloud_answers_optimized.rs), [performance report](evidence/2026-10-15-resource-cloud-code-review/perf-report.md).

## 클라우드 기능 정상성 — 실제 검사와 남은 범위

기존 관련 검사 3개 명령을 canonical 현재 소스에서 실제 실행해 모두 exit0를 받았다. MCP 내부 subprocess의 nested1 PASS는 중복 제외했다. 새 회귀는 별도 tests-only scratch에서 현재 제품 로직을 바꾸지 않고 실행했다.

| 실행한 기존 검사 | PASS | ignored |
| --- | ---: | ---: |
| App `cloud_agent::` | 161 | 3 |
| App `cloud_history_worker::` | 6 | 0 |
| `agent-mcp` | 67 | 1 |
| `web-remote` unit | 388 | 1 |
| web integration/contract | 20 | 12 |
| 합계 | **642** | **17** |

실행 명령과 개별 exit code는 [test-results](evidence/2026-10-15-resource-cloud-code-review/root-test-results.json), 중복 제거는 [집계](evidence/2026-10-15-resource-cloud-code-review/root-existing-test-counts.json)에 있다. 이전 작업의5702 PASS를 이번 재리뷰 결과로 재사용하지 않았고, 이번 전체 workspace 검사를 수행했다고 주장하지 않는다.

| 기능 | 이번에 확인한 동작 | 실제 PASS 근거 예시 |
| --- | --- | --- |
| HTTP/OAuth 인증→PTY 입력→출력/자체 답변 | private real PTY로 연결된 도구 경로 동작 | `oauth_http_to_real_pty_output_and_own_answer_roundtrip`, `pr9_http_private_pty_unicode_paste_and_own_answer_roundtrip` |
| 연결별 승인/읽기 scope/단일 writer | named 승인으로 legacy 권한이 확대되지 않음 | `granting_only_a_named_connection_does_not_share_with_the_legacy_bearer`, `explicit_consent_and_one_writer_are_required_for_new_credentials` |
| 세션 독립성/입력 재검사 | 원래 UUID/generation/실행/권한/lease 검증 | `same_external_id_has_two_independent_original_session_answers_and_receipts`, `pr7_pending_claim_rechecks_revoke_expiry_and_exact_target` |
| durable claim와 PTY receipt | 저장 claim 전 dispatch 금지·unknown 자동 재시도 금지 | `pr7_claim_completion_is_required_before_input_dispatch`, `pr7_durable_claim_exists_at_effect_and_admission_unknown_is_not_retried` |
| notify/답변 저장 | commit 전 게시하지 않고 원래 세션 유지 | `pr7_locked_finish_does_not_block_or_publish_uncommitted_answer`, `pr9_notify_own_answer_stays_original_after_switch_and_connection_stop` |
| run 상태/heartbeat/cancel | 요청 수락과 작업 완료·agent 취소 ACK를 구분 | `operator_cancel_waits_for_durability_and_same_actor_ack`, `heartbeat_loss_persists_unknown_and_cannot_be_reported_completed_directly` |
| 이력 읽기 | actor/target cursor 분리·byte cap·wrapped secret redaction | `history_is_correlated_bounded_redacted_and_cursor_cannot_cross_actor`, `history_redacts_known_secret_across_terminal_wraps_before_publication` |
| 일반 종료/credential 교체 | queued input revoke·접수된 답변 finish drain | `pr7_shutdown_cancels_pending_input_claim_and_drains_accepted_answer_finish`, `auth_rotation_retires_old_named_permissions_without_exhausting_capacity` |
| PWA 제어/탐색 | operator epoch/scope 재검사·원래 socket으로만 완료 전달 | `cloud_control_mailbox_rechecks_revocation_epoch_and_permissions_at_drain`, `cloud_navigation_completion_returns_only_to_requesting_websocket` |
| 연결 표시/재접속/터널 cleanup | listener와 실제 인증을 구분·retry opt-in·owned child reap | `setup_state_local_listener_does_not_claim_public_or_client_connection`, `retry_is_opt_in_capped_and_explicit_stop_cancels_it`, `cancellation_reaps_only_owned_child_before_startup_finishes` |

정상 fixture 경로는 위 범위에서 동작한다. **실제 dot/Meta Muse/Grok Bot 등 계정 연결·provider 도구 준수·public tunnel·실제 모바일 기기/브라우저·OS sleep은 이번에 검증하지 않았다.** 17 ignored를 PASS로 세지 않는다. Relay production BLOCKED, Windows 의존성 미캐시 등 앞선 명시 한계도 해소했다고 주장하지 않는다. 실제 사용자 계정에 작업을 전송하지 않았다.

추가 검토한 Codex cancellation/queue, HistoryWorker 32 jobs/4MiB budget, ended-session loader receiver 유지, tunnel update coalescing, MCP bounded handlers, PTY IO cancellation/join, runtime bounded final drain/cache에서는 새 영구 heap/worker 누수의 구체적 도달 경로를 확정하지 못했다. 프로세스 adjacency 최적화는 이전 수정으로 확인했으며 다시 병목 항목에 넣지 않았다. 이 문장이 전체 서비스 무결함 보증은 아니다.

## 권장 구현 순서와 PR 경계

| PR | 범위 | 완료 기준 |
| --- | --- | --- |
| 1 | R01 종료 scheduling 차단 + R05 bootstrap retry | 지속 오류에서도 유계 종료·복구 후 durable run 표시·정상 durable finish 보존 |
| 2 | R02 일반 전경 workload 정리 | normal/frozen HUP 무시 job 종료·PID/PGID 재사용 보호·IO join 보존 |
| 3 | R03 outward session metadata redaction | title/workspace_name·late registration 마스킹·routing identity 유지 |
| 4 | R04 direct WS 전체 인증 시한 | 정상 fragment 성공·시한 후 valid auth 거절·stop/slot release |
| 5 | R06 종료 세션 읽기 승인 철회 | actual checkbox 해제·다른 세션 승인 유지·종료 input 거부 |
| 6 | P01 revision 기반 안전한 projection cache | unchanged Arc 재사용·corpus 변경 즉시 redaction·동일 부하 실측 비교 |

아직 제품 코드를 수정하지 않았다. 구현 시 이 리뷰의 synthetic RED를 production 회귀로 옮기고 영향 검사부터 실행한다. 사용자용 업데이트를 빌드/배포할 때는 현재 last-shipped보다 높은 version과 binary/bundle version 검증이 필요하며, 현재 task에는 앱 재실행 승인이 없다.

## 후속 수정 — 2026-10-15

이 리뷰의 R01–R06/P01을 이후 병렬 수정했다. [수정·실제 검증 보고서](../qa/2026-10-15-resource-cloud-fixes.md)와 [최종 evidence](../qa/evidence/2026-10-15-resource-cloud-fixes/README.md)를 참고한다. 위 RED·측정과 미수정 상태 표시는 리뷰 완료 당시 기록으로 유지한다.
