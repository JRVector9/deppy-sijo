# 워크스페이스 환경 및 API 개선 구현 계획

**목표:** 프로젝트 환경파일과 API 비밀값을 확인·편집하고 새 Agent 실행에 일관되게 전달한다.

**설계:** 원본 환경파일, Keychain의 비밀값, workspace 소속의 연결 메타데이터를 분리한다. 파일 원본과 실제 적용값을 구분하며 UI 렌더링은 worker가 만든 상태만 소비한다. 기존 프로세스 환경을 외부에서 변경했다고 표시하지 않는다.

**기술:** Rust, egui, SQLite, macOS Keychain, 기존 bounded settings/dotenv worker.

**권한/범위:** 사용자가 PR 단위 정리와 구현 착수를 요청했다. PR 1부터 순차 구현한다. 이번 단계에서 앱을 패키징하거나 재실행하지 않는다. 기존 `fix/settings-copy-session-background` 미커밋 화면 수정은 별도로 유지한다. main 자동 머지는 하지 않는다.

## PR 1 — 설정 조회 상태와 오류 수명 수정

- GitHub PR: #179 (draft), 구현 commit `7f3fb81`.
- 브랜치: `fix/environment-snapshot-state`, base `main`.
- 파일: `crates/app/src/settings_snapshot.rs`, `main.rs`, `app.rs`, `ui/env_profiles.rs`, `ui/credentials.rs`.
- [x] 기존 코드에서 정상 snapshot 수신 후 오류가 남는 상태 전이 테스트를 실패시킨다.
- [x] Loading/Ready/Failed 상태를 분리하고 초기화·프로젝트 전환에는 Loading을 전달한다.
- [x] 정상 snapshot은 조회 오류만 지우며 저장·삭제·비밀 공개 오류는 보존한다. 빈 목록은 정상 Ready다.
- [x] worker가 실패 snapshot을 반환해도 재시도하고 오래된 workspace 결과를 적용하지 않는다.
- [x] 조회 실패 로그는 고정된 단계/코드만 남기고 DB 값/자격증명을 노출하지 않는다.
- [x] `cargo test -p deppy-sijo --bin deppy-sijo settings_load_ -- --test-threads=1`로 상태 로직을 검증한다. UI 화면 검증은 사용자 재실행 승인 뒤 진행한다.
- [x] 최종 소스 재리뷰와 게이트 결과 기록 완료. commit/draft PR로 화면 확인을 요청한다.

## PR 2 — 파일 삭제와 동기화 상태 일관성

- 구현 완료: #180 (draft), commit `49ad36f`. 화면 검증 대기.

- 브랜치: `fix/environment-file-consistency`, base PR 1.
- 파일: `crates/app/src/dotenv_sync.rs`, `app.rs`, `ui/env_profiles.rs`, 로케일 5개.
- 동일 키가 `.env`와 `.env.local` 모두에 있을 때 병합 목록의 삭제 의미를 명시한다. 파일별 값 제거를 선택할 수 있게 하고 전체 제거는 모든 정의를 대상으로 한다.
- 여러 파일 기록은 전체 입력 검증·원본 변경 확인 후 실행한다. 중간 I/O 실패를 성공으로 보고하지 않으며 일부 반영 상태를 재조회한다. 원본을 무조건 덮는 rollback은 하지 않는다.
- 파일이 전부 없어지면 복구용 DB 값은 보존하되 화면에 보관값/원본 없음으로 구분한다. 실제 새 실행값이 빈 상태라는 사실을 표시한다.
- 검증: 같은 파일 중복, 두 파일 중복, 파일 부재, 읽기/쓰기 실패, 작업 중 외부 수정, workspace 전환. 실행 명령 `cargo test -p deppy-sijo --bin deppy-sijo dotenv_sync -- --test-threads=1`.

## PR 3 — API credential과 Agent 환경 연결

- 구현 완료: #181 (draft), commit `b21aec5`. API 연결이 있는 프로젝트는 eval 기반 셸 라이브 반영을 끄며 새 실행부터 적용한다.

- 브랜치: `feat/workspace-credential-env`, base PR 2.
- 파일: `crates/storage/src/db.rs` 및 기존 migration 체계, `crates/app/src/app.rs`, `ui/credentials.rs`, `crates/runtime/src/in_process.rs`, 로케일 5개.
- workspace/환경변수명/credential ID 연결을 DB에 저장한다. API 키 추가/기존 키 편집에서 환경변수명을 명시한다. provider 문자열만 보고 자동 인증 성공으로 간주하지 않는다.
- 비밀값은 Keychain에 유지하고 연결만으로 .env에 평문을 기록하지 않는다. 일반 dotenv < 명시적인 workspace API 연결 < 실행용 profile 순서로 충돌을 해결하고 사용자에게 충돌을 보여준다.
- 소유 workspace 또는 명시적 전역 공유 credential만 연결한다. 참조 중인 키 삭제는 차단하거나 연결 해제 후 처리한다. DB migration 실패/Keychain 실패 시 부분 연결을 만들지 않는다.
- 새 Agent와 새 셸 실행 시 최신 연결을 확인해 기존 secret resolution/lease 경로로 전달한다. 비활성 workspace, 복원, quick launch도 동일 계약을 적용한다.
- 검증: workspace 격리, env 이름 검증, 중복 충돌, 변경/해제/삭제, Keychain 실패, spawn 주입. 명령 `cargo test -p storage credential_env` 및 `cargo test -p runtime credential_env`와 app 연결 테스트.

## PR 4 — 환경파일 선택과 출처

- 구현 완료: #182 (draft), commit `2b2c6c8`. 첫 범위는 루트의 영문·숫자·._- 파일명 16개까지다. 하위 경로는 지원하지 않는다. 빈 목록과 루트 연결 해제는 복원에서도 파일을 주입하지 않는다.

- 브랜치: `feat/workspace-env-sources`, base PR 3.
- 파일: storage migration, `runtime/src/dotenv.rs`, `app/src/dotenv_sync.rs`, `app.rs`, `ui/env_profiles.rs`, watcher 연결, 로케일 5개.
- 기본 `.env`→`.env.local` 유지. 사용자가 추가 파일과 적용 순서를 지정한다. 개발/운영 파일을 발견했다는 이유만으로 동시에 주입하지 않는다.
- workspace 루트 기준 상대경로를 저장하고 원본 파일/우선순위/덮어쓴 값 여부를 행별로 제공한다. 추가·수정은 선택한 파일에 기록한다.
- 초기 범위는 dotenv 형식이다. 임의 JSON/TOML 및 자동 셸 코드 실행은 포함하지 않는다. 기존 parser의 지원 문법을 명시하고 읽기와 쓰기의 왕복 일관성을 보장한다.
- 기존 바이트/항목/경로·symlink 제한을 모든 선택 파일의 합계에 적용한다. active watcher와 spawn/restore 재조회가 같은 목록을 사용한다.
- 검증: 순서 변경, 명시적 활성화, 경로 이탈/심볼릭 링크, 파일 삭제, 예산 초과, 선택 파일 편집/재실행. 명령 `cargo test -p runtime dotenv` 및 `cargo test -p deppy-sijo --bin deppy-sijo dotenv_sync`.

## PR 5 — Agent 적용 상태

- 구현·리뷰 반영·로직 검증 완료, draft PR 생성 단계. 화면 검증 대기. 실제 spawn ACK와 기본환경 처리 ACK를 구분하고, 비밀값 대신 physical slot 세대를 버전에 포함한다.

- 브랜치: `feat/environment-application-status`, base PR 4.
- 파일: `runtime/src/command.rs`, `runtime/src/event.rs`, `in_process.rs`, `app.rs`, 환경 설정 snapshot/UI, 로케일 5개.
- workspace별 설정 revision과 실행 시 전달된 revision을 기록한다. 화면은 저장됨/동기화 실패/새 실행부터 적용/세션 실행 당시 버전을 구분한다.
- 이미 실행 중인 Agent에 환경을 주입했다고 표시하지 않는다. 갱신은 새 세션 실행 또는 사용자가 선택한 재시작으로 수행한다. 자동으로 세션을 종료하지 않는다.
- 실험적 zsh live reload는 셸 프롬프트 범위임을 표시하며 Agent 반영과 구분한다. 삭제된 값과 parser 차이도 별도 회귀 검증한다.
- 검증: revision 불일치, 실패 후 성공, workspace 이동, 복원, 실행 중 편집. 새 상태 로직 테스트와 사용자 화면 검증을 나눈다.

## 통합과 검증

- 각 PR의 소스 변경·리뷰·테스트 결과를 handoff에 기록한다. 미실행 테스트와 화면 확인은 PASS로 기록하지 않는다.
- 로직/저장은 테스트 먼저, 화면 레이아웃 변경은 사용자 승인 후 빌드·화면 확인한다.
- PR 번호는 실제 생성 후 기록한다. PR 1~5는 현재 순서 식별자이며 GitHub 번호가 아니다.
