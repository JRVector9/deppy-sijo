# 승인된 통합 모바일 PWA 구현 검증

기준 시안: ../mockups/pwa-mobile-unified-input-2026-10-10.html
설계: ../superpowers/specs/2026-10-10-pwa-unified-mobile-design.md
계획: ../superpowers/plans/2026-10-10-pwa-unified-mobile.md
시작 소스: d8445b5c, 앱 버전0.9.0. 이 작업은 소스 구현이며 실행 빌드를 배포하거나 앱을 재시작하지 않는다.

## PR와 담당 기록

| 단위 | 담당 모델 | 소유 영역 | 검증 상태 | 커밋 |
|---|---|---|---|---|
| 계획/시안 | root | 계획·설계·HTML·handoff | staged diff check 실제 PASS | 055bcec4 |
| PR1 이력 API | unified_backend · gpt-6.1-sol/xhigh | Rust terminal/runtime/web history | 구현·리뷰·회귀 통과 | 9edb041b |
| PR2 렌더러 | unified_renderer · gpt-6.1-sol/xhigh | viewer-core JS/CSS·독립 fixture | 구현·리뷰·회귀 통과 | 0a0f01cd |
| PR3 모바일 UI | unified_mobile_ui · gpt-6.1-sol/xhigh | 루프백/Relay shell·theme·독립 fixture | 구현·리뷰·회귀·시안 대조 통과 | 80db4539 |
| PR4 통합 | root | 러너·기존 회귀·코드 리뷰·QA/handoff | 전체 Rust·브라우저10개·리뷰·시안 대조 통과 | 본 문서 최종 커밋 |

서브에이전트는 도구의 지원 모델 gpt-6.1-sol와 xhigh를 명시해 실제 spawn 성공했다. 다른 모델/낮은 effort로 대체하지 않았다. 추가 스킬은 사용하지 않는다.

## 시안 대조 체크리스트

모든 항목은 아래 실제 실행 기록과 production-assets 화면 대조를 근거로 완료했다. 설치형 PWA 실기기는 별도 미검증 범위다.

- [x] 터미널/작성 모드 버튼과 두 입력 DOM이 사라지고 입력창 하나만 표시된다.
- [x] 기본1줄/44px/16px 입력, 최대5줄 후 내부 스크롤, 첨부/보조 키/전송 배치가 시안과 같다.
- [x] Enter/Shift+Enter/IME 조합/긴 한글/NFD 경로/거절·초안 복구/ACK/세션 전환·종료 회귀를 검증한다.
- [x] 상단 과거 열람 바와 예약44px 행이 없다. 과거 열람 때만 하단 맨 아래로 버튼이 나타난다.
- [x] 충분한 이력이 있는 작은 native 화면에서 모바일 높이가 실제 출력으로 채워진다.
- [x] 연속 과거 조회/overlap/cache trim/anchor/selection/search/font/keyboard 변경 후 읽던 행이 유지된다.
- [x] 확대된 live 표시가 native 한 화면으로 축소되지 않는다. 늦은 응답·다른 세션·다른 generation 결과를 거절한다.
- [x] Mac PTY rows/cursor/native scroll/damage/alternate screen을 조회가 변경하지 않는다.
- [x] 워크스페이스 기본 펼침/접힘/검색/갱신 상태와 전체 폭76px 세션 행,14px2줄 제목·12px agent/status가 시안과 같다.
- [x] 비활성 세션 선택의 워크스페이스 전환 안내와 current/live/dormant 구분이 정확하다.
- [x] 세션 선택/설정 하단 패널의 표제/옵션/초안 보존/포커스·ESC·배경 닫기를 검증한다.
- [x]320×568,390×844,844×390 및 가상 visualViewport 키보드/입력 높이 변화에서 화면 bounds를 유지한다.
- [x] Relay의 지원하는 읽기 UI는 동일한 구성이고 쓰기 권한/allowlist는 확대하지 않는다.

## 실행 기록

- 모델: 세 agent spawn 모두 `gpt-6.1-sol/xhigh` 실제 성공. 세 read-only Codex CLI 리뷰 헤더도 동일 모델/effort를 확인했다. 다른 모델/스킬 적용 없음.
- 초기 backend37PASS: `/private/tmp/deppy-unified-backend-green-20261010.log`. terminal12, runtime8, web17 (실제 인증 WebSocket/임시 worker 포함). 최초11PASS1FAIL은 새 native 출력 뒤 blank행 변경을 허용하지 않은 test oracle; stable scrollback prefix와 완전 역사12행 equality로 수정했다.
- 전체 단위: `/private/tmp/deppy-unified-rust-full-20261010.log`, terminal133PASS4ignored, runtime379PASS, storage426PASS. App `--lib`는 binary-only 패키지에 잘못 사용한 명령이라 실패했으며 다음 올바른 명령으로 실행했다.
- App: `/private/tmp/deppy-unified-app-strict-20261010.log`, main2817PASS38ignored, integration39PASS/3ignored. terminal/runtime/storage/web/App5패키지 all-target `clippy -D warnings`, workspace fmt check PASS. 잘못된 xtask `boundary` 명령은 기록하고 올바른 `check-boundary`로 재실행해 PASS했다.
- 최종 Web: `/private/tmp/deppy-unified-backend-review-green-20261010.log` exit0, lib364PASS1ignored, 새로운 native-generation/unsupported-history 실제 WebSocket 회귀 각1PASS, check-boundary PASS.
- 최종 브라우저·엄격 검사: `/private/tmp/deppy-unified-browser-final-20261010.log` exit0. 9개 Chromium target의10개 테스트 모두 PASS: 신규 unified renderer/UI2개, 기존 mobile-shell/usability, viewer-history/reading/readability/grapheme/core, Relay. 마지막 수정이 들어간 web/App all-target Clippy·전체 fmt·check-boundary도 PASS.
- 마지막 시각 보정 후 재실행: `/private/tmp/deppy-unified-visual-final-20261010.log` exit0, unified2 + mobile-shell/usability + Relay5개 테스트 PASS. dropdown⌄ 보완 후 `/private/tmp/deppy-unified-picker-final-20261010.log` exit0, unified2 + mobile-shell + Relay4개 테스트 PASS. 변경과 관련된 최종 CSS도 실제 Chromium으로 검증했다.
- 독립 Chromium의 실제 상세 검증: renderer111checks, unified UI95checks (최종 CSS 보정 전77, viewport 보정 후93), legacy usability187assertions(저장 초안 재로드 포함), history89/reading182/readability113checks PASS. Relay Node16PASS와 실제 Relay Chromium wrapper PASS. Node 테스트는 암호화/재연결 계약이고 Relay 시각 증거와 구분한다.
- 독립 코드 리뷰: 모든 CLI에 `-m gpt-6.1-sol -c model_reasoning_effort=xhigh`를 지정하고 헤더에서 확인했다. Rust2medium, JS high+medium을 재현·수정했다. 수정된 unsupported-history native 복귀/native-generation fence/disjoint passive 읽기 보존/reconnect rows-dedupe4건을 독립 재검토한 `/private/tmp/deppy-unified-fix-review-20261010.txt`는 exit0/OK, 잔여 발견 없음.
- 추가 실제 회귀: latest-follow 버튼 오표시, 수동 overlap stale blank, pending-layout 뒤 사용자의 과거 읽기 의도/재시도 덮임, source expiry automatic retry, sent-only row dedupe, 빠른 font-change debounce, IME 중 interrupt/특수키/늦은 이벤트를 수정·검증했다.
- Producer identity: storage `pane-1` != `session-live` SQL/async projection assertion은 full426 suite에 포함해 PASS했다. preflight/retained heap에 optional session UUID를 포함했다. suspended는 저장된 sessions.id, active는 fresh IdMap, warm은 실제 mux UUID를 사용하고 제목/index/local u64를 연결 기준으로 사용하지 않는다.

## 시각 검증

실제 production index/core/theme/app assets와 격리된 FakeSocket으로390×844 화면을 생성해 승인 시안과 대조했다. 터미널 가용 높이가 이력 출력으로 채워지고, 동일 입력창의5줄 확장과 설정 하단 패널을 확인했다. 워크스페이스의 초기 잘림은 headless Chrome 최소500px layout을390px screenshot으로 촬영한 harness 문제였다. CDP390px emulation으로 재촬영하여 innerWidth/clientWidth/scrollWidth가 모두390px이고 전체 행·우측 화살표가 정상 범위임을 확인했다. 실제 visualViewport가 layout viewport보다 작을 때 헤더 칩의 vw 상한이 과대해질 수 있는 문제는 UI 담당이 실제 viewer/Relay 폭 기준42% 상한과320/390 bounds 회귀로 보정했다. 설정 버튼44px/7px 간격과 연결 상태 레이블의8px 간격을 대조했고, 승인 시안의 세션 선택의⌄ 표시와 title truncation용26px 오른쪽 여백을 추가했다. 최종 UI95checks PASS다. 아래 임시 파일은 실제 소스 렌더 결과이며 실서비스·실기기 연결 화면은 아니다.

- `/private/tmp/deppy-unified-actual-terminal-20261010.png`
- `/private/tmp/deppy-unified-actual-workspace-20261010.png`
- `/private/tmp/deppy-unified-actual-long-20261010.png`
- `/private/tmp/deppy-unified-actual-settings-20261010.png`

초기 스크린샷의 비어 있는 terminal은 테스트 fixture의 의도적인 held-history 플래그를 preview가 그대로 물려받은 결과였다. preview 플래그를 해제해 정상 이력 채움을 확인했다. 실제 제품 결함이나 테스트 PASS로 오기하지 않는다.


## 재현 명령

같은 체크아웃에서 Cargo를 직렬로 실행한다. 프로젝트별 dependency/cache가 이미 준비된 이 작업 환경의 명령이다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p web-remote --lib
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -q -p web-remote --test pwa_unified_chrome --test mobile_usability_chrome --test mobile_shell_chrome --test viewer_history_chrome --test viewer_reading_chrome --test viewer_readability_chrome --test viewer_grapheme_chrome --test viewer_core_chrome --test relay_shell_chrome -- --ignored --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py clippy -q -p web-remote -p deppy-sijo --all-targets -- -D warnings
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py fmt --all -- --check
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py run -q -p xtask -- check-boundary
node --test web/relay-shell/tests/*.test.mjs
```

## 실기기와 배포 구분

격리된 실제 Chromium 테스트와 로컬 임시 PTY/인증 WebSocket 테스트는 실제 모바일 hardware 테스트를 대신하지 않는다. iPhone/Safari·Android 설치 PWA 실기기·시스템 키보드/clipboard/longpress를 직접 검증했는지는 마지막에 별도로 기록한다. 이 작업에서 제품 실행 빌드를 전달하지 않으므로 버전은0.9.0이며, 후속 출시 빌드는 증가한 버전과 binary/bundle 검증이 필요하다.
