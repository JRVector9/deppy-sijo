# 프로젝트 키 관리 보고서·시안 수용 검증

기준 보고서: [원인·개선 보고서](../investigations/2026-10-12-project-key-management-review.md). 승인 시안: [HTML](../mockups/project-keys-proposal-2026-10-12.html). 화면 대조: [시안과 제품 렌더 비교](../mockups/project-keys-implementation-comparison-2026-10-12.html).

기준 소스 `ff722082 / 0.10.3`, 구현 `feat/project-key-management-20261012`. 아래 PASS는 실행한 격리 테스트의 결과다. 사용자 Keychain·원본 파일·GitHub·클립보드를 바꾸는 실계정 시험은 하지 않았다. production adapter와 실제 제품 렌더 함수를 사용하고 비밀 저장소/clipboard/원격은 주입 가능한 대체 IO로 검사한다. 원본 파일, 실제 child/PTY, SQLite 장애·재시작·host-exit 테스트는 임시 경로에서 실행했다. 실행 중인 앱은 재시작하지 않았다.

## 요구사항별 증거

테스트 이름은 고유한 끝부분으로 표기한다. 전체 실행 결과와 명령은 아래 기록한다.

| PR | 요구 사항 | 구현 위치 | 실행한 검증 증거 | 상태 |
| --- | --- | --- | --- | --- |
| 1 | 환경파일·수동 API 통합, 중복 출처·우선순위 유지 | core/storage, service, Environment 분기 | `distinct_sources_keep_duplicate_names_visible_with_explicit_precedence`, `legacy_reconciliation_tracks_advanced_names_disconnects_and_new_rows`, 실제 표 fixture | PASS |
| 1 | 원본·사용처·버전·로컬/원격 상태 구분 | UI 표·상세·변경 이력 | `table_origins_exact_44_rows_and_28_actions`, `saved_timestamp_is_distinct_from_latest_metadata_change`, same-revision remote ACK 회귀 | PASS |
| 1 | 수정·빈 수정값 유지·실패 시 재입력 | edit dialog, fenced worker/service | `blank_edit_keeps_existing_credential_ids_source_usage_and_revision`, `failed_replacement_requires_reentry_and_preserves_editor_metadata`, 값 slot/CAS 격리 | PASS |
| 1 | 값 더블클릭·버튼 복사, 현재 버전 확인 | NativeClipboard 주입·copy worker | `value_only_double_click_emits_copy_and_name_click_selects`, `explicit_copy_intent_has_exact_key_revision_and_no_optimistic_notice`, slow/stale clipboard·value rotation fixture; OS clipboard는 대체 IO | PASS |
| 1 | 공유 키·기존 agent 인증·context-prefill 보존 | App/legacy advanced adapters | shared-reference detach·unregistered import·late writer fixture, `project_keys_explicit_profile_preserves_short_configuration_and_precedence`, context-prefill UI | PASS |
| 2 | 프로젝트·개발/운영 독립 | DB scope/ownership/CAS | `environment_ownership_duplicates_and_cas_are_enforced`, production migration retention·retired provenance·source mutation runtime ACK 격리 | PASS |
| 2 | 보관함·dotenv·PEM·GitHub-only 구분 | domain validators/service | remote-only metadata/store-read=0, 실제 dotenv 및 PEM fixture, source type 검증 | PASS |
| 2 | 파일 선택/import/PEM 연결 실제 intent | source dialog·worker·source IO | `selected_source_preview_ack_drives_exact_import_intent`, `explicit_source_import_reuses_existing_legacy_row_and_selected_binding`, PEM custom-name import | PASS |
| 2 | 화면과 무관한 감시·atomic rename·중복 이벤트 | 독립 worker source poll·metadata revision | `independent_enrollment_and_restart_scan_both_environments_without_ui_selection`, file generation·staged rollback fixture | PASS |
| 2 | worker 재생성·잠자기 복귀 재검사 | interval source poll·fresh scan | 위 restart fixture와 최신 원본 직접 읽기 fixture; 지속 poll이 긴 중단 뒤 재검사, OS 잠자기 자체는 재현하지 않음 | PASS |
| 2 | 누락·빈 원본·잠김·실패 시 last-good 유지 | service/storage state machine | `last_good_survives_missing_empty_failure_and_conflict`, fresh empty-name enrollment·optional locked/plain fixture | PASS |
| 2 | 동시 수정·늦은 응답·명시적 충돌 해결 | opening-revision CAS·worker·UI | `source_editor_keeps_opening_revision_and_production_default`, `conflict_and_missing_source_have_real_repair_intents`, late-scope/request ACK | PASS |
| 2 | staged secret/file write·DB 실패·orphan 복구 | protected ledger·full-file journal | mark-applied/value-publication DB trigger 실패 회귀, dead-owner journal recovery, root/parent replacement rejection, oldwriter guarded upsert/prune | PASS |
| 3 | 프로젝트·환경별 빌드 명령 저장/수정 | recipe DB·dialog·service | `build_recipe_save_keeps_argument_boundaries_and_scope`, `recipe_editor_round_trips_empty_and_multiline_arguments` | PASS |
| 3 | 앱 실행·CLI env run·정확한 인수 | main early dispatch·native child | CLI saved-recipe exact argv/cwd fixture, `version_and_help_are_pure_dispatch_before_paths_and_secret_store`, actual child/foreground PTY stdin | PASS |
| 3 | 최신 원본 preflight·실행 snapshot 고정 | anchored sources·build snapshot | `file_missing_restore_conflict_and_build_github_use_real_published_generations`, `build_preflight_enrolls_new_selected_sources_without_watching_or_ui`, immutable scoped receipt | PASS |
| 3 | 필수값·충돌 거부, optional 누락 제외 | build preflight·runtime exclusions | disconnected-required fixture, actual child ambient/other-environment scrub, optional locked/plain fixture | PASS |
| 3 | PEM 제한 권한·정상/취소/앱 종료 정리 | temp file·native process group·shutdown | custom PEM 0700/0600 fixture, actual host exit, timeout/descendant pipes/cancel, receipt SQLite가 막힌 동안 먼저 PEM 정리 | PASS |
| 3 | 사용 이름·버전·시각·결과, secret 없는 영수증 | receipts DB·UI | immutable scoped receipt, dead-owner abandoned receipt recovery, newest-five/running-recipe UI, schema/Debug secret-free fixture | PASS |
| 4 | gh/저장 token, repo/environment/org 목적지 | auth choice·target dialog·transport | exact account/scope/opt-in UI, staged auth bundle, environment URI encoding, numeric organization repo IDs, wrong-account rejection | PASS |
| 4 | 공개키 암호화 전송·평문 argv/log 제외 | crypto_box sealed box·gh api PUT | `github_encrypted_put_preserves_newlines_and_numeric_organization_identities`: 실제 ciphertext 복호화 일치, CRLF·64비트 ID·stdin/argv 확인 | PASS |
| 4 | 명시적 자동 전송·수동 전송·retry | target opt-in·durable outbox | `github_config_save_preserves_exact_auth_scope_and_opt_in`, external-change overwrite intent, old publication auto-queue recovery | PASS |
| 4 | 실패·restart·backoff·latest-only queue | storage lease/outbox·worker | `outbox_retry_and_expired_claim_survive_restart`, coalesce/CAS, auth/network failure; 90초 whole-operation deadline <120초 lease | PASS |
| 4 | 늦은 ACK의 새 revision/target 덮어쓰기 방지 | exact lease/target/revision fence | pre-send expiry/new-generation test, outbox stale-ACK test, remote observer active-lease/after-ACK fixture | PASS |
| 4 | 외부 변경·불명확한 PUT 자동 전송 보류 | remote metadata CAS·transport | post-PUT metadata 미확인·응답 소실·nonzero 결과 pause, inactive scope/remote-only observer·concurrent ACK fence | PASS |
| 4 | remote-only/개인키 원격 재조회 오표시 방지 | source capabilities·UI·store IO | `new_remote_only_source_saves_metadata_without_secret_and_keeps_config_action`, remote-only copy/send read-only and zero secret reads | PASS |
| 5 | 44pt 행·28pt 행 버튼·34pt 버튼·36pt 필드 | actual egui geometry | table rect test·editor field/footer test | PASS |
| 5 | 300pt 상세 폭·좁은 화면 아래 배치·scroll | actual egui layout | narrow outer-scroll exposes detail/build and real recipe intent, Korean 390pt viewport rect test | PASS |
| 5 | 480pt 편집·공용 modal·내부 scroll·고정 footer | connector-ui popup + app reexport | `edit_popup_uses_480_shell_36_fields_34_footer_and_narrow_internal_scroll`; 공용 사례44–49/49개 번호·HTML scripts 검사 | PASS |
| 5 | 키보드·실제 IME·focus fencing·locale | MaskedSecretBuffer·dialog·App fence | 실제 Enabled/Preedit/Commit/Disabled Korean IME, overflow selected-value/cursor 보존, AccessKit/undo 숨김, five-locale174키 동일 | PASS |
| 5 | 전체 흐름·secret 비노출·resource bounds | isolated service/worker/transport/UI harness | scoped generation·source/build/GitHub fixture, Slow clipboard/long build responsive actions, max keys/sources/fields/argv/ciphertext bounds | PASS |
| 5 | 독립 실제 diff 리뷰 후 지적 수정 | gpt-6.1-sol / xhigh CLI review + root audit | domain1/2, UI, backend17항목, GitHub review; 실제 RED→GREEN 회귀. 검색 누락·production dependencies도 정상 빌드 검사로 보완 | PASS |
| 5 | 전체 회귀·normal build·boundary/deps·strict lint | workspace serial Cargo gate | 최종 check/nextest5366/strict lint/boundary/deps/fmt 모두 exit0, isolated PNG export1PASS | PASS |

## 최종 검증 실행

- 제품 소스 동결 hash manifest: `/private/tmp/deppy-project-keys-frozen-source-20261012.json`.
- 최종 전체 로그: `/private/tmp/deppy-project-keys-final-workspace-verified-20261012.log` (7개 gate 모두 exit0).
- 최종 전체 회귀: **5366 tests run / 5366 passed / 63 skipped**, 실패0. skipped는 기존 CI 제외와 명시적 수동 export 등이며 전체 테스트를 모두 실행했다고 표현하지 않는다.
- strict workspace all-targets clippy(`-D warnings`), normal workspace check, 구조 경계 및 의존성 검사, fmt 모두 PASS.
- isolated actual UI PNG export: **1 passed / 0 failed**, desktop1360×900·narrow390×900·edit800×700 생성 및 직접 시각 검토. 제품42파일 hash가 검증 전후 일치했다.
- 실행 명령(cwd `/Users/jr/Desktop/projects/deppy-sijo-project-keys`):

```sh
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py check --workspace --locked
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py nextest run --workspace --locked --profile ci
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py clippy --workspace --all-targets --locked -- -D warnings
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py run -p xtask --locked -- check-boundary
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py run -p xtask --locked -- check-deps
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py fmt --all --check
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py test -p deppy-sijo --bin deppy-sijo ui::project_keys::tests::render_project_keys_design_review -- --ignored --exact
```

실제 PNG export는 전체 테스트 경로의 exact filter로 실행했다. [데스크톱](assets/project-keys-2026-10-12/desktop.png), [좁은 화면](assets/project-keys-2026-10-12/narrow.png), [수정 창](assets/project-keys-2026-10-12/edit.png)을 승인 HTML과 직접 비교했고, 기존 geometry/intent 테스트에서 44/28/34/36pt·300/480pt 규격과 스크롤·고정 footer를 확인했다. 예시 데이터 개수와 내용은 기능 검증용이며 실계정 값이 아니다. 조사/테스트 build이며 사용자가 쓸 앱 bundle을 배포하지 않는다. 제품 릴리스 때는 0.10.3보다 큰 minor version과 바이너리·bundle version을 별도로 검증한다.

## 실패한 접근과 최종 수정

- 최초 normal binary check에서 crypto_box/base64가 테스트 의존성에만 있어 실패했다. production dependencies로 옮겨 실제 정상 빌드를 검증했다.
- 첫 전체 gate는 cargo-nextest 미설치로 중단했으며 테스트 실패로 세지 않는다. 공식 CI 동일 버전0.9.148을 작업용 private 경로에만 준비했다. global 설치·서명 검증 통과를 주장하지 않는다.
- 첫 전체 nextest는5360 run/5354PASS/6FAIL/63skipped였다. 기존 legacy dotenv 갱신·기본 전달 회귀4는 unowned projection과 실제 관리 소유권을 구분해 해결했다. memo source-law marker 충돌1과 protocol28 assertion 잔류1도 수정했다. 추가 재검토로 운영 프로필과 metadata-only Keep 보존 회귀까지 보완했다.
- 재실행5366PASS 후 strict lint 지적3(동등한 let-chain2/테스트 모듈 배치1)을 정리하고 최종 전체 gate를 다시 실행했다. 마지막 run의 검증 소스는 변경하지 않았다.
- HTML 검사 스크립트의 inventory 변수명 가정(`cases`/실제`caseGroups`)을 고쳐49개 번호를 재검사했다. 제품 HTML 동작 실패가 아니다.

## 독립 리뷰와 추가 회귀

| 검사 | 실제 결과 / 로그 |
| --- | --- |
| Domain 최종 focused | 최종 storage47PASS + strict core/storage all-targets lintPASS; 최종 workspace의 storage479 unit+8 integration도 PASS, oldwriter/profile-coordinate/metadata-only Keep 실제 RED→GREEN |
| App source/build/native | source9/build4/process6PASS; actual host-exit/foreground PTY·PEM cleanup 순서 확인 |
| GitHub transport | 9PASS, case-sensitive identity/Secret 비교 실제 RED→GREEN; encrypted payload 복호화 검사 |
| UI 최종 focused | 33PASS(실제 IME 포함)/1 PNG-export ignored; 공유 MaskedSecretBuffer undo 회귀 RED→GREEN |
| 기존 설정 호환 | 최종 호환 batch151PASS/1ignored: exact profile Configuration·ordinary migration·late writer·pending journal·Production profile retention·metadata-only Keep 포함 |
| 설정 검색 | actual RED0PASS1FAIL→GREEN8PASS, 새/고급 화면 실제 nav click 회귀도 최종 workspace에서 PASS: `/private/tmp/deppy-project-keys-settings-search-{red,green}-20261012.log` |
| 정상 binary 의존성 | non-test check 실제 missing base64/crypto_box RED; production dependencies 이동 완료, 최종 workspace check 및 strict lint PASS |
| HTML·locale | 승인/공용 HTML inline script node check, unique IDs 승인44/공용16/비교4; numbered popup49개, 모든5locale의 project_keys174키 동일 PASS |

회귀 로그는 `/private/tmp/deppy-project-keys-*-20261012.log`, CLI 리뷰는 `/private/tmp/deppy-project-keys-*-review*-result-20261012.txt`에 보관했다. 처음 실패한 시도도 남기며 0개 테스트 filter를 PASS 근거로 사용하지 않는다. 운영 GitHub 평문을 다시 읽는 기능과 실행 중인 셸의 환경변수 변경은 제공하지 않는 API 계약이다.
