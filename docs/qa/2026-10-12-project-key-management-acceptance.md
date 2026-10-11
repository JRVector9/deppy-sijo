# 프로젝트 키 관리 보고서·시안 수용 검증

기준 보고서: `docs/investigations/2026-10-12-project-key-management-review.md`.
승인 시안: `docs/mockups/project-keys-proposal-2026-10-12.html`.
구현 브랜치: `feat/project-key-management-20261012`, 기준 `ff722082 / 0.10.3`.

실제 실행·검증된 결과만 PASS로 바꾼다. 이 표의 대기는 미완료이며, 코드가 있다는 이유만으로 검증 완료로 표시하지 않는다. 사용자 Keychain, 프로젝트 파일, GitHub, 빌드, 현재 앱은 테스트에서 변경하지 않는다.

| PR | 요구 사항 | 구현·검증할 위치 | 실제 실행 증거 | 상태 |
| --- | --- | --- | --- | --- |
| 1 | 기존 환경파일·수동 API가 중복 출처를 유지하며 통합 목록에 표시 | core/storage import, worker snapshot, 실제 설정 분기 | 대기 | 대기 |
| 1 | 원본·사용처·보관 버전·저장/원격 상태 구분 | UI 표·선택 상세 | 대기 | 대기 |
| 1 | 명시적 수정, 기존 값 유지(빈 수정 입력), 실패 시 창 유지 | UI edit intent + worker CAS | 대기 | 대기 |
| 1 | 숨긴 값 영역 더블클릭·복사 버튼으로 실제 OS clipboard 작업 | copy intent/result project/revision fence | 대기 | 대기 |
| 1 | 공유 키·기존 agent 인증·context-prefill 경로 보존 | legacy advanced adapter | 대기 | 대기 |
| 2 | 같은 이름의 프로젝트·개발/운영 키가 독립적 | bounded metadata DB queries/ownership | 대기 | 대기 |
| 2 | 앱 보관·dotenv·PEM·GitHub-only 원본 구분 | domain validators/service | 대기 | 대기 |
| 2 | 파일 선택/import/PEM 연결이 실제 작업으로 연결 | UI intent + source worker | 대기 | 대기 |
| 2 | 활성 설정 화면과 무관한 감시, atomic rename, 중복 이벤트 | service poll/fresh source snapshot | 대기 | 대기 |
| 2 | 앱 재생성·잠자기 복귀 뒤 재검사 | fake clock/worker recreation | 대기 | 대기 |
| 2 | 누락·빈 원본·잠김·실패에서 last-good 보존 | fake secret store/source fixtures | 대기 | 대기 |
| 2 | 동시 수정·늦은 결과 충돌 표시·명시적 해결 | source/CAS conflict UI + worker | 대기 | 대기 |
| 2 | secret bundle staged writes·DB 실패·orphan cleanup 복구 | isolated ledger/store tests | 대기 | 대기 |
| 3 | 프로젝트·환경별 빌드 이름/명령/cwd 저장·수정 | recipes DB/UI/service | 대기 | 대기 |
| 3 | 앱 실행 및 실제 CLI env run 문법 연결 | main dispatch, fake child | 대기 | 대기 |
| 3 | build preflight는 최신 원본, 실행 snapshot은 고정 | injectable process/temporary sources | 대기 | 대기 |
| 3 | 필수 키 실패·미해결 충돌 시 실행 거부, optional 규칙 | preflight tests | 대기 | 대기 |
| 3 | PEM 경로/임시 파일 제한 권한·종료 정리 | temporary file/process tests | 대기 | 대기 |
| 3 | 사용 키 이름·버전·실행 시각·결과 영수증, secret 제외 | receipts DB/UI/tests | 대기 | 대기 |
| 4 | gh 계정/저장 token 참조 및 repo/environment/org 범위 지정 | target form/domain/transport | 대기 | 대기 |
| 4 | 실제 암호화 outbound 호출, 평문 argv/log 제외 | fake gh process stdin/argv assertions | 대기 | 대기 |
| 4 | 자동 전송 명시적 opt-in, 수동 전송·retry | UI intent + durable outbox | 대기 | 대기 |
| 4 | auth/network 실패·restart 복구·backoff·latest-only queue | fake transport/storage queue tests | 대기 | 대기 |
| 4 | 늦은 ACK가 새 로컬 revision/target 상태를 덮지 못함 | leased target/revision fencing | 대기 | 대기 |
| 4 | 외부 원격 수정 시 값 미확인·자동 전송 보류 | remote timestamp metadata tests | 대기 | 대기 |
| 4 | remote-only/Pem private key는 원격값 재조회처럼 표시 안 함 | UI/model copy/send capability tests | 대기 | 대기 |
| 5 | 44pt row, 28pt row action, 34pt button, 36pt input | egui headless actual rect assertions | 대기 | 대기 |
| 5 | 300pt 선택 상세·좁은 창 아래 배치·내부 scroll | viewport fixture/layout tests | 대기 | 대기 |
| 5 | 480pt 편집 및 공용 modal/body/footer, popup 사례 문서 | shared popup components/tests | 대기 | 대기 |
| 5 | 키보드·IME 입력·modal focus fencing·모든 locale 키 | bounded text buffer and i18n tests | 대기 | 대기 |
| 5 | 사용자 전체 흐름·secret 미노출·resource bounds | fake DB/store/source/remote end-to-end harness | 대기 | 대기 |
| 5 | 실제 product diff 독립 CLI 리뷰 후 지적 보완 | gpt-6.1-sol / xhigh review log | 대기 | 대기 |
| 5 | workspace boundary/fmt/clippy/관련 회귀 완료 | 직렬 Cargo gate 실행 결과 | 대기 | 대기 |

검증 gate(cwd 신규 worktree):

```sh
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py test -p deppy-core project_keys
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py test -p storage project_keys
python3 /private/tmp/deppy-project-keys-cargo-gate-20261012.py test -p deppy-sijo --bin deppy-sijo project_keys
```

각 gate는 기존 performance gate와 같은 flock을 사용하고 `performance/target` cache를 공유한다. build/test는 앱을 실행하지 않는다. 제품 번들을 전달할 경우 minor 버전 증가와 실제 번들 버전 검증을 별도 기록한다.

## 실제 중간 실행 결과

- 담당 실행 `test -p storage project_keys`: 11 PASS. `project_keys_environment_ownership_duplicates_and_cas_are_enforced`, `project_keys_last_good_survives_missing_empty_failure_and_conflict`, `project_keys_legacy_introduction_keeps_override_rows_and_never_reads_plaintext`, `project_keys_settings_are_independent_and_bounded`, `project_keys_credential_references_block_cleanup_and_owner_moves`, outbox coalescing/late ACK/external change, retry/expired claim/restart, immutable scoped receipt, atomic value publication rollback, append-only migration, metadata serialization/Debug secret-free 검증.
- 이 결과는 storage 계층만 검증한 것이다. worker/실제 UI/전체 흐름 검증이 완료되기 전 위 수용 행을 최종 PASS로 표시하지 않는다.
- root actual code 독립 CLI 리뷰 시작: `/private/tmp/deppy-project-keys-domain-review-result-20261012.txt`, model `gpt-6.1-sol`, effort `xhigh`. 결과 확정 전 리뷰 통과 주장 없음.
- 1차 root CLIreview exit0, `CONCLUSION: issues`: production/dev legacy import isolation, stale remote pause overwrite(High); full-capacity UPSERT, restored last-good fencing, authchoices cap(Medium). 모두 담당에게 regression+수정 요청. 2차 review/최종tests 대기.
- storage 전체448 unit/8 integration/0doctest PASS(owned pointer tightening 전). 이후 scoped core3/storage16 PASS, strict clippy(core/storage alltargets -Dwarnings) PASS. 리뷰 수정 후 최종 다시 검증해야 한다.
- connector-ui actual masked input undo regression RED 0PASS/1FAIL; 기존 password widget이 plaintext undo를 유지했다. 수정·GREEN 대기, UI 완료로 표시하지 않는다.
- root shared popup HTML49case uniqueIDs 및 node --check exit0, git diff --check exit0 실행. 이는 문서 시안 syntax 검증이며 product UI를 대신하지 않는다.
- UI focused GREEN18PASS/0FAIL/1ignored(별도 PNG export), common secret_input undo GREEN1PASS. 보고서 전체 수용표는 backend/UI 통합·최종 시각 대조 후 갱신한다.
- Backend actual service4fixture PASS: 독립 dev/prod 저장, 최신 원본·충돌·복구·build/gh stdin, 새 dotenv key/quote/폴더교체 차단, PEM 권한/사용/정리. source2/processgroup timeout PASS. mixed filter에 UI tests도 포함되었으므로 App 전체 검증 완료가 아니다.
- Root `xtask check-deps` PASS(crate27), `check-boundary` FAIL3: cli/worker 구체 Keyring 생성, service Db::open이 app.rs composition boundary 밖에 있었다. backend에 factory injection 이동 요청; boundary 규칙은 완화하지 않는다. 수정 후 재실행 대기.
- Domain 최신 focused27PASS: 1차 리뷰5결함 및 auth scope/old pointer generation 등 실제 회귀 포함. pending queue recovery와 selected legacy delete 보완 후 전체회귀·2차 root review 대기.
