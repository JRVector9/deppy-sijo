# 메인 에이전트 목록 중복 수정

사용자 화면에서 `claude`가 반복되며 여러 행이 모두 선택 강조를 받는 현상을 재현했다. 실제 실행 catalog는 provider/model/effort 조합별로 구성된다. 호스트가 이를 그대로 provider 목록으로 투영하고 native 설정이 중복 문자열을 각각 그려 동일 값의 모든 행이 선택 상태가 됐다.

호스트의 선택 축(provider·model·effort)을 각각 중복 없이 제공한다. 실제 `available_agents`의 모델·추론 강도 조합은 보존한다. native 설정은 이전 호스트/중복 데이터도 안전하게 처리하도록 원래 순서를 유지하며 같은 값은 한 번씩 그린다. 고급 실행 규칙의 작업자 체크박스에도 동일하게 적용한다. 브라우저의 메인 제공자 선택은 이미 중복 제거를 했으며, 공유 호스트 수정은 브라우저 작업자 목록에도 적용된다.

실제 RED: native 테스트가 선택 행2개/기대1개로 실패했고, 호스트 테스트도 claude3개/codex2개를 반환해 실패했다. 수정 후 실제 native 드롭다운의 항목 수, 제공자 변경에 따른 모델·추론 강도 연결과 메인 바인딩 해제, 작업자 체크박스1개씩, 자동 저장/실행/전송 없음까지 통과했다.

| 실행한 명령 | 결과 |
| --- | --- |
| `CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo command_center -- --test-threads=1` | 146통과·0실패·1ignored |
| `node web/shared/tests/command-center.browser.mjs` | Chrome237검증통과 |
| `CARGO_BUILD_JOBS=2 cargo clippy --offline --locked -p deppy-sijo --bin deppy-sijo --tests -- -D warnings` | exit0 |
| `cargo fmt --all -- --check` · `git diff --check` | exit0 |

전체 workspace 테스트나 사용자 설정 변경·실제 에이전트 실행은 수행하지 않았다. [로그와 소스 SHA](assets/command-center-provider-duplicates-20261020/source.json)에 검사 경계를 보존했다. 별도 기능 브랜치의 소스 수정이며 현재 서명된0.15.0 앱에는 후속 수정이 포함되지 않는다. 이번 수정 요청에는 새 재실행 요청이 없으므로 앱은 교체하지 않았다. 사용자용 수정 빌드 전달 시 patch 버전을0.15.1 이상으로 올리고 CLI와 bundle 두 버전을 검증해야 한다.
