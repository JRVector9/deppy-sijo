# 환경 및 API 새 화면 구현 계획

**Goal:** 승인한 목록·세로 입력 패널을 적용하고 기존 화면으로 즉시 되돌릴 수 있게 한다.

**Architecture:** 기존 EnvProfilesUi/CredentialsUi의 초안, 유계 비밀값, snapshot, intent를 재사용한다. 새 renderer는 별도 UI 모듈로 추가하고 기존 contents_compact를 보존한다. ui.environment_classic_view 설정만 추가하며 DB/Keychain/환경파일 저장 경로는 바꾸지 않는다.

**Tech Stack:** Rust, egui, 기존 config TOML/locale 5개.

## 1. 복구 설정

- [x] config.rs: UiConfig에 기본 false인 environment_classic_view bool을 serde(default)로 추가한다.
- [x] config.rs의 environment_view_config_roundtrip에서 이전 설정의 기본값, true 저장/로드, false 복귀를 검증한다. 먼저 없는 필드 compile RED를 확인한다.
- [x] app.rs: 관리 closure 앞에서 토글을 캡처하고 closure 뒤에 바뀐 값만 pending_config_save로 저장한다. 설정 변경은 화면 선택에만 영향을 준다.

## 2. 표시 모듈

- [x] ui/environment.rs: EnvironmentUi { tab, search, drawer, workspace }를 추가한다. 전체/API/환경변수/파일 탭과 추가 버튼, 오른쪽 패널(좁은 창에서는 패널만)을 구성한다.
- [x] ui/credentials/modern.rs: 기존 CredentialsUi를 확장하여 prepare/목록/추가 패널/완료 정리 메서드를 제공한다. 공급자 선택, API 이름, 선택 환경변수 연결, 마스킹된 비밀값을 같은 CredentialsIntent::Add로 보낸다. 공개/삭제/연결 편집은 기존 경로를 유지한다.
- [x] ui/env_profiles/modern.rs: 기존 EnvProfilesUi를 확장하여 prepare/목록/파일/추가 패널 메서드를 제공한다. DotenvWrite와 SetSources 계약을 재사용한다. 파일 읽기 실패/원본 부재, 레거시 데이터, 삭제 확인을 숨기지 않는다.
- [x] app.rs: 기존 프로젝트 rail/header를 유지하고 bool에 따라 새 renderer 또는 기존 renderer를 호출한다. 두 화면 모두 동일 snapshot과 worker 완료 상태를 쓴다.
- [x] 화면 전환/프로젝트 변경 시 비밀값과 미저장 초안을 지워 다른 프로젝트로 옮겨가지 않게 한다. API 추가 진행 상태는 기존 완료 ACK로만 해제한다.

## 3. locale·검증·리뷰

- [x] 새 화면 문구를 locale 5개에 같은 키 집합으로 추가한다. API 이름/환경변수 이름/API 키 값을 구분한다.
- [ ] 사용자 이전 규칙에 따라 앱 패키징·재실행은 구현이 준비된 뒤 승인받는다. UI 회귀 테스트를 추가하지 않는다. config 저장 동작은 해당 단일 테스트로 확인한다.
- [x] 커밋 전 fmt, workspace clippy all-targets, check-boundary, i18n, diff 게이트를 한 번 수행한다. 실패를 수정한 경우에만 필요한 검사를 재실행한다.
- [x] 실제 코드 diff를 codex CLI로 리뷰하고 지적을 반영한다. 한국어 commit, main 대상 PR, Obsidian 프로젝트 일지와 handoff를 기록한다.

## 4. 우클릭 시안만 보완

- [x] HTML의 우클릭을 API 이름/API 키 값과 환경변수 이름/값으로 분리한다.
- [x] API 이름 선택은 표시 이름에, API 키 값 선택은 비밀 입력에 채워지는지 브라우저에서 확인한다. 실제 터미널 동작은 이번 구현에 포함하지 않는다.

## 복구 명령

화면의 기존 화면 전환을 사용한다. 앱을 열 수 없다면 기존 config.toml의 [ui]에서 environment_classic_view = true로 지정한 뒤 다음 실행에 적용한다. 저장 스키마를 바꾸지 않아 main 9432d33의 기존 패키지로 돌아가도 이 작업 때문에 데이터 변환이 필요하지 않다.
