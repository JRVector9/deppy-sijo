| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| medium | crates/app/src/ui/credentials.rs: BindingDraft / render_env_bindings | 동적 입력 위젯의 실행 취소 기록이 초안 삭제 뒤 잔류 | 프로젝트·API 변경 누적에 따라 불필요한 메모리 보관 | 초안과 위젯 상태를 함께 소유하도록 수정·검증 |
| medium | crates/app/src/ui/credentials.rs / ui/env_profiles.rs: clear_sensitive_string | 내용을 지운 뒤 대형 입력 allocation이 계속 유지 | 거절·취소한 입력의 큰 메모리가 장기간 상주 | 0 덮기 뒤 allocation도 반환하도록 수정·검증 |
| medium | crates/app/src/ui/credentials.rs: complete_add | 이전 API 저장 응답이 같은 프로젝트의 새 초안을 초기화 | 서비스·연결 이름 소실 및 잘못된 키 연결 가능 | 저장 요청과 현재 초안의 관계를 추적하도록 수정·검증 |
| medium | crates/app/src/ui/env_profiles.rs: reset_var_form / ui/credentials/modern.rs: reset_modern_draft | 변수 이름·API 서비스/연결 이름의 undo가 다음 초안에 혼입 | 새 값에 이전 프로젝트의 이름이 결합될 수 있음 | reset·prefill·저장·Drop에서 위젯 기록도 정리 |

## Details

대상은 `fix/environment-api-context-integration`의 환경/API 통합 소스다. 시작 HEAD는 f16d4e9(소스 73cc041)이며, 운영 DB·사용자 비밀값을 읽거나 실행 앱을 변경하지 않았다. 독립 Codex CLI는 실제 소스와 egui 0.36.1 구현을 읽어 두 번째 지적과 세 번째 지적의 변수 이름 경로를 확인했다. 첫 번째 및 동일 원인의 서비스/연결 이름은 자체 검토로 확인했다.

### 메모리 보관·해제

- egui `TextEditState`의 undo는 Context의 IdTypeMap에 보관된다. 기존 연결 목록은 credential별 동적 위젯 ID를 만들지만 `HashMap<String, String>`만 비웠다. 의존성의 직렬화 예산은 실행 중 map의 위젯 상태를 자동 해제하는 보장이 아니다.
- `BindingDraft`가 값과 기존 `DraftTextEditState`를 함께 소유한다. snapshot 갱신/연결 저장/화면 닫힘에서 초안을 제거하면 공유 undo를 비우고 해당 위젯 상태도 제거한다. 무관한 TextEditState를 일괄 삭제하지 않는다.
- 고정 ID의 변수 이름·API 서비스명·API 연결 변수명에도 같은 추적을 연결했다. 기존 API 이름/비밀값 정리를 보존하고 서비스 프리셋을 바꾸는 경우에도 종전 서비스/연결 이름의 undo를 폐기한다.
- 기존 사후 byte truncate 대상인 metadata·환경값 입력에는 입력 시점 char_limit도 적용했다. API 비밀값은 한도 초과 시 거절하는 기존 계약을 보존한다. char_limit은 문자 기준이고, 저장의 기존 byte 한도는 그대로 별도 적용한다.
- 큰 비밀 입력을 취소/거절해도 String::clear가 capacity를 유지하는 별도 보관 문제를 확인했다. 0 덮기와 compiler_fence 이후 빈 String으로 교체해 allocation을 반환한다. 상태 테스트는 수정 전 API 2,097,152byte 및 env 65,536byte 잔류로 실패하고 수정 후 capacity 0을 확인했다. 운영체제 RSS가 즉시 같은 크기로 감소한다는 보장은 아니다.
- 확인한 Context clone 경로는 소유 개수가 제한되고 UI 상태를 Context 안에 역참조로 보관하지 않는다. 이 범위에서 Arc 순환 누수는 확인하지 못했다. Galley 캐시에는 프레임별 정리가 있다. 앱 전체의 누수 부재나 실제 RSS 감소를 입증했다는 뜻은 아니다.

### 저장 응답

- API 저장 중 `+ 추가`/취소/prefill로 초안을 교체하면 이전 요청이 현재 초안의 소유자가 아님을 표시한다. 이전 ACK는 작업 대기만 해제하고 새 provider·label·env_name·비밀값·undo·오류를 보존한다.
- 같은 초안을 유지한 성공/실패 ACK는 기존대로 초기화/저장 실패를 표시한다. `add_pending`은 초안 교체로 해제하지 않아 중복 저장 방지를 유지한다. 요청 ID나 저장 worker·DB 계약은 변경하지 않았다.

### 검증

- 최초 RED는 새 타입/메서드 부재 컴파일 실패. scaffold 후 실제 assertion RED는 1 PASS/4 FAIL: 잔류 binding 초안, metadata undo 2종, 새 provider 소실. 수정 후 5 PASS.
- 최종 신규 상태 테스트 8 PASS: 위 회귀와 현재 ACK 성공/실패 보존, 128회 credential 변경(동적 위젯 ID 512개) 누적 검사. 매 회차 관련 TextEditState가 제거되고 무관한 편집기 상태 1개는 유지됨을 검사했다. 큰 입력 allocation 반환 2건도 포함한다. 실행 0.02초. 장시간 GUI/RSS 측정과 구분한다.
- 기존 관련 상태 테스트 17 PASS: environment_context 10, pr188 3, 늦은 secret 응답 1, 다른 프로젝트 저장 ACK 1, 로딩 중 선택 유지 1, 환경 프로젝트 숨김 격리 1. 신규와 합계 25 PASS이며 전체 workspace 테스트를 실행했다는 뜻은 아니다.
- 신규 명령: `CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo test --locked -p deppy-sijo --bin deppy-sijo environment_memory_ -- --test-threads=1`.
- 로그: `/tmp/deppy-env-memory-{red,behavior-red,green,verified,related}-20260912.log`. 최종 로그는 `/tmp/deppy-env-memory-final-tests-20260912.log` 및 `final-related-20260912.log`이다. 기존 상태 테스트는 최종 컴파일한 test binary에서 위 필터로 순차 실행했다.
- 독립 최초 리뷰: `/tmp/deppy-env-memory-review-20260912.log`. 수정 후 소스 리뷰 `/tmp/deppy-env-memory-fix-review-20260912.log`는 새 확실한 문제 없음으로 완료됐다. 마지막 allocation 반환 두 함수는 `/tmp/deppy-env-memory-capacity-review-20260912.log`에서 별도 소스 재확인했고 이중 해제·이동된 값 훼손 등 새 문제는 없었다. 독립 리뷰는 테스트를 실행하지 않았고 위 PASS는 주 에이전트가 실제 실행한 결과다. 커밋 직전 gate 5개는 모두 exit 0: fmt, strict workspace/all-target clippy, check-boundary, i18n-check, diff --check. 로그 `/tmp/deppy-env-memory-final-gates-20260912.log`. i18n 필터의 0건 실행 suite는 테스트 수에 합산하지 않았다.

### 적용 상태

이번 요청에서 release 재빌드·재실행·main 머지는 하지 않는다. 직전 빌드된 bundle은 소스 73cc041 기준이며 이번 수정은 포함하지 않는다. 현재 실행 앱도 별도 기존 attention 7442a8f bundle이다. 실제 화면 및 장시간 사용 확인은 새 소스 빌드/실행 후 필요하다.
