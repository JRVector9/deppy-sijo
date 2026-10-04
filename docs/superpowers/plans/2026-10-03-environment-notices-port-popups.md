# 환경 설정·안내·포트 공용 팝업 적용 계획

**Goal:** 승인된 팝업 디자인을 사례09·15·33·34·35·37에 적용하고 갱신한 HTML 시안을 브라우저에 표시한다.

**Architecture:** 호출자가 확인 대상·초안·명령을 유지한다. 환경 확인 표시를 `ui/environment_dialogs.rs`에 모으고, 세 상태 안내는 `ui/popup/information.rs`를 재사용한다. 상태바 포트 목록은 앵커 팝오버를 유지하고 모달과 동일한 머리글·팔레트·푸터 및 목록 행을 사용한다. 네이티브 설정창과 AI 런처는 기존 구조다.

**Tech Stack:** Rust, egui0.36.1, egui_kittest, 기존 five-locale i18n, 단일 HTML 시안.

작업 방식: 현재 세션에서 직접 순서대로 실행한다. 사용자의 기존 디자인 승인과 이번 적용 지시를 사용하며 추가 디자인 승인·서브에이전트·자동 커밋을 요구하지 않는다. Workstep.md가 없으므로 workstep의 소스 리뷰 부분을 적용한다. 앱 재실행은 이번에 요청되지 않았다.

## 표시 계약

- 09:400pt 공용 확인창, 프로젝트 이름과 기존 세션 유지 설명. 원래 프로젝트ID를 캡처해 닫기/취소 결정만 반환한다.
- 15:480pt 공용 확인창, 변수 이름·경고·36pt 원본 선택 입력·34pt 삭제/취소 버튼. 기본 Enter는 삭제하지 않고 Esc/X/바깥 클릭은 취소한다. 원본 파일 선택과 기존 `DotenvWrite` 경로를 유지한다.
- 33–35:420pt 공용 안내창, 정보 notice·닫기 버튼. 한 pass에는 대기 안내 하나만 표시하며 예정 안내는 배경 키 처리 전에 입력 fence에 포함한다.
- 37:최대560pt 앵커 팝오버, 공용 머리글과 닫기·스크롤 본문·고정 새로고침 푸터. 현재/다른/외부 섹션, IPv4/IPv6 소켓 문자열, 주소 복사, 소유한 프로세스만 종료를 유지한다. 위험 동작은 기존12번 확인창에서만 실행한다.
- 공통:18/13/12/11pt 글꼴,34pt 버튼,36pt 선택 입력,1pt 구분선, 좁은 화면의 줄바꿈·스크롤. 고정 사례ID로 캐시를 한정한다.

## 1. 시안과 재사용 API

Files: `docs/mockups/shared-popup-components-2026-09-30.html`, `docs/design/popup-components.md`, `crates/app/src/ui/popup/{shell,fields,mod}.rs`; Create:`popup/information.rs`,`popup/list.rs`.

- [x] 시안09·15의 실제 문구/파일 선택,33–35의 안내,37의 상세 포트 목록을 갱신한다.42개 기존 번호는 유지한다. `#terminal_status`로 직접 열 수 있게 한다.
- [x] 기존 셸에서 머리글과 기본 스타일을 재사용할 수 있도록 분리한다. 모달 렌더링은 기존 입력 소유권을 유지한다.
- [x] 추가 표시 API:

```rust
pub fn information(ctx: &egui::Context, spec: InformationSpec<'_>) -> bool;
pub fn popover(ui: &mut egui::Ui, spec: PopupSpec<'_>, contents: impl FnOnce(&mut egui::Ui)) -> bool;
pub fn popover_frame(ctx: &egui::Context) -> egui::Frame;
pub fn list_row<T>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> T) -> T;
pub fn choice_input<R>(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, selected: &str, contents: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<Option<R>>;
```

## 2. 환경 설정09·15

Files: `crates/app/src/ui/{environment_dialogs,env_profiles,mod}.rs`, `crates/app/src/app.rs`; Test:`env_profiles.rs`의 실제 확인 렌더러 하네스.

- [x] 먼저 기존15번의 버튼 높이·Esc 취소·원본 보존을 검증하는 테스트를 추가하고 실제 RED를 기록한다.
- [x] `environment_dialogs::project_close`와 `delete_variable`은 공용 표시와 결정만 반환한다. 기존 App/EnvProfilesUi가 상태와 명령을 소유한다.
- [x] 확인 대상ID 변경 시 포커스를 초기화하고 작업 의도가 이미 있을 때 삭제 의도를 덮어쓰지 않는다. 설정 카테고리 전환으로 확인창을 숨기지 않도록 호출 위치를 확인한다.
- [x] `env.delete_source` 한 키를 다섯 언어에 추가한다.
- [x] GREEN: 실제 렌더링·Esc·기본Enter·명시적삭제·선택 파일 보존 검증.

## 3. 안내33–35

Files: `crates/app/src/app.rs`,`ui/popup/information.rs`; Test:`information.rs` 하네스.

- [x] 각각 기존 지역화된 제목/본문과 고정ID를 새 정보 표시 컴포넌트에 연결한다.
- [x] 공용 안내창의34pt 닫기, 긴 본문 줄바꿈, 좁은 화면 경계,Esc/X/버튼 닫기와 pass 입력 차단을 검증한다.
- [x] App의 대기 판정에 세 안내 상태를 포함한다. 큐 상태를 임의로 지우거나 의미를 바꾸지 않는다.

## 4. 포트 목록37

Files:`crates/app/src/ui/{ports,agent_terminal}.rs`; Test:`ports.rs`의 실제 manager 하네스.

- [x] 먼저 갱신/복사/종료 버튼 규격과 좁은 목록 경계를 검증하는 테스트를 추가해 실제 RED를 기록한다.
- [x] 공용 셸 스타일·머리글·목록 행·푸터를 적용하고 기존 종료 확인은 팝오버 외부에 유지한다.
- [x] GREEN: 새로고침은 의도만 생성, 복사는 소켓 그대로, 외부프로세스 종료 없음, 종료 대상 보존, 포트 재열기 시 한 번 갱신, Esc/바깥 클릭/닫기와 스크롤 확인.

## 5. 검증·리뷰·빌드

- [x] `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo env_ports_popup -- --test-threads=1` (RED/GREEN 로그 분리).
- [x] `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=8` 및 `cargo test --offline --locked -q -p i18n`; 실제 실행 결과를 기록한다.
- [x] `cargo clippy --offline --locked -p deppy-sijo --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check`.
- [x] 실제 UI 오프스크린 PNG와 HTML을 검증하고 시안을 기본 브라우저에 표시한다. Deppy는 실행하지 않는다.
- [x] 이번 작업 시작 스냅샷 `/tmp/deppy-env-ports-popup-before-20261003`과의 실제 코드 diff를 Codex CLI로 리뷰하고 관련 지적을 수정한다.
- [x] 이전 버전/번들 최대치를 확인해 patch 버전을 올리고 오프라인 locked 릴리스 빌드와 별도 버전 번들을 만든다. 상속된 워크스페이스 버전·컴파일 버전·macOS 두 버전 필드·패키지 일치를 검증한다.
- [x] 디자인 계약·번호별 상태·최종 보고서·handoff를 갱신한다. 커밋/푸시/재실행은 별도 요청 대상이다.

## Self-review

사용자 지정6사례(09·15·33·34·35·37)를 모두 작업2–4에 매핑했다.37번의 포트 부분만 적용하고 다른 승인·리소스 팝오버의 표시 구조는 이번 범위 밖이다. 기존12번 포트 종료 확인창을 재구현하지 않는다. 원본 파일 선택과 포트 소유권 검증은 호출자/기존 서비스에 남는다.
