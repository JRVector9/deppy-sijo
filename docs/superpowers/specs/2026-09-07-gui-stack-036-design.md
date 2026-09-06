# GUI 의존성 0.36 통합 이관

## 목표와 범위

별개 Dependabot PR #142(egui), #153(egui_commonmark)을 하나의 호환 가능한
GUI 의존성 변경으로 대체한다. eframe/egui/egui_extras/egui_kittest는 0.36,
egui_commonmark는 0.25로 함께 이동한다. 잠금 파일에는 하나의 egui 계열만 남긴다.
사용자가 승인한 추가 개발이며 독립 worktree에서 구현·검증·리뷰·PR까지 진행한다.
앱 실행 파일 빌드와 재실행은 하지 않고 화면 검증을 통과했다고 주장하지 않는다.

## 결정

- 개별 crate만 올리면 0.35/0.36 타입과 renderer가 공존하므로 전체 GUI 계열을 함께 올린다.
- 터미널 IMEOutput은 새 purpose 필드에 IMEPurpose::Terminal을 명시한다.
  기존 한글 조합 소유권, 진행 중 Preedit 보호, 후보창 위치 계약은 보존한다.
- RawInput.modifiers 대신 Event::ModifiersChanged를 입력 이벤트 앞에 넣는다.
  수정키의 프레임 상태와 Key 이벤트의 modifiers는 각각 원래 의미를 유지한다.
- eframe 0.36의 실제 winit 의존성을 검사하고 기존 winit 0.30.13 macOS IME
  backport를 유지한다. 업데이트가 동등한 upstream fix를 포함하지 않는 한 제거하지 않는다.
- Markdown은 default-features=false, commonmark의 pulldown_cmark와 extras의 image만
  활성화한다. 외부 file/http loader는 켜지 않는다. 동일 pulldown-cmark 파서로 목적지를
  해석하고 로컬 PNG broker의 경계·상한 및 링크 intent 경로를 유지한다.
- 저장, PTY, wire, 레이아웃과 폰트 정책은 변경하지 않는다. 새 API로 필요한 최소 수정만 한다.

## 검증과 배포 경계

의존성 이관 뒤 컴파일 실패를 API RED 증거로 기록하고, 터미널 IME 목적의 동작 회귀는
잘못된 Normal 목적에서 실패 후 Terminal로 통과하는 것을 확인한다. terminal 전체,
app --bin 전체, 나머지 workspace 테스트, workspace all-target clippy, fmt,
xtask check-boundary/check-deps/i18n 검사, Markdown 이미지·링크 회귀를 실행한다.
앱 통합 테스트는 제품 실행 파일을 자동 빌드하므로 해당 파일을 독립 rustc 테스트로
검사할 수 있는 경우에만 실행하며 제외한 항목은 기록한다. CARGO_BUILD_JOBS=2와
전용 target을 사용한다. 최종 게이트 전에 최신 origin/main을 일반 merge로 반영한다.

Codex CLI 리뷰의 지적을 반영한 한국어 커밋을 push하고 replacement PR을 만든 다음
#142/#153을 닫는다. 머지·배포·실행·force-push는 이 작업에 포함하지 않는다.

## 근거

- https://github.com/emilk/egui/blob/0.36.1/CHANGELOG.md
- https://github.com/lampsitter/egui_commonmark/blob/v0.25.0/CHANGELOG.md
- third_party/winit-0.30.13/DEPPY_BACKPORT.md
