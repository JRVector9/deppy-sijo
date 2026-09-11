# 환경 및 API 통합과 터미널 선택 연결

기존 승인 시안(#188)과 최신 사용자 피드백을 구현한다. main 머지와 실행 앱 교체는 별도 사용자 요청으로 진행한다.

## 완료 기준
- 에이전트 대기 수정 7442a8f와 환경 새 화면 d301882를 한 브랜치에 통합한다. 기존 화면 토글을 보존한다.
- 설정을 새로 열면 현재 터미널 워크스페이스가 선택된다. 설정 안에서 고른 다른 프로젝트는 편집 중 유지한다. 숨긴 현재 프로젝트도 설정 목록에 다시 표시한다.
- 터미널 선택 우클릭에서 API 이름/키 값, 환경변수 이름/값으로 채운다. 원본 pane의 프로젝트를 사용하고 저장은 기존 양식에서 확인한다.
- 평문을 로그/설정에 남기지 않는다. 입력 한도를 넘으면 비활성화하며 새 초안은 이전 초안과 섞지 않는다.

## 단계
1. 일반 merge로 #188 통합. app/locale 자동 병합, handoff 양쪽 기록 보존.
2. app.rs 설정 진입 선택 상태를 closed→open 전이로 갱신. loading 중 선택 유지 회귀와 재진입 회귀 확인.
3. ui/environment.rs에 일회성 prefill, workspace.rs에 선택 메뉴/세션 출처 intent, app.rs에 원본 workspace 라우팅을 연결. credentials/modern.rs와 env_profiles/modern.rs의 기존 입력 초안만 채운다.
4. API 이름과 선택된 값이 바로 보이도록 입력 양식 표시를 보완. 프로젝트 목록이 선택 행을 보이게 스크롤한다. 저장/worker/DB 스키마 계약 변경 없음.
5. 화면 밖 상태/입력 한도/초안 정리 회귀 먼저 실행. 실제 Codex CLI 소스 리뷰와 지적 수정 후 커밋 직전 fmt/clippy/boundary/i18n/diff gate 한 번.
6. handoff와 프로젝트 일지/한국어 커밋. 재빌드·재기동은 기존 사용자 승인 규칙을 따른다. 화면 검증 전 GUI PASS로 보고하지 않는다.

## 명령
공통 환경: CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target
- cargo test --locked -p deppy-sijo --bin deppy-sijo environment_context_ -- --test-threads=1
- cargo fmt --all -- --check
- cargo clippy --locked --workspace --all-targets -- -D warnings
- cargo run --locked -p xtask -- check-boundary
- cargo run --locked -p xtask -- i18n-check
- git diff --check
