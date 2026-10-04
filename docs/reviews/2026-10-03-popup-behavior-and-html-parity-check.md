# 팝업 동작·시안 일치 점검 — 2026-10-03

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | `docs/mockups/shared-popup-components-2026-09-30.html:53` · 37 | 미리보기 높이보다 포트 팝오버가 커서 푸터가 잘림 | 시안에서 닫기·새로고침 버튼을 볼 수 없음 | 미리보기 컨테이너의 남은 높이로 팝오버를 제한하고 본문만 스크롤 |
| low | 같은 HTML:76·103 · 09·15·33–35 | 첫 notice의 추가 margin과 빈 subtitle이 실제 구현에 없는 여백 생성 | 동일한 내용의 시안이 구현보다 약16–23px 높음 | 첫 notice margin을 없애고 빈 subtitle 숨김으로 공용 계약과 맞춤 |

## 결론과 범위

최근 적용한09·15·33·34·35·37과 연결되는 기존12번 종료 확인, 공용 입력/포커스 회귀를 확인했다. **이번 검증에서 실제 앱 컴포넌트의 동작 오류는 재현되지 않았다.** 시안의 외형은 완전히 같지 않으며 위2건은 실제 브라우저에서 확인한 미해결 사항이다. 제품 코드·HTML은 이 점검에서 수정하지 않았다.

- 현재 작업 소스/산출물 버전0.5.5. 제품 diff SHA256 `891a6a2312db9c90172fbce2bd3e664c140b48157f0a156043de9f6fd84ca847`이 이전 릴리스 기록과 같음을 실제 재계산했다.
- Deppy 종료/실행/재실행·배포·커밋·푸시 없음. 네이티브 사용 중인 앱 GUI E2E가 아니라 실제 egui 컴포넌트 하네스와 화면 밖 GPU 렌더링으로 확인했다. 사용자 파일 삭제·프로세스 종료·클립보드 쓰기를 수행하지 않았다.
- Python Playwright 모듈이 없어 이미 설치된 Node Playwright와 캐시된 Chromium으로 동일한 headless 검증을 수행했다. 새 설치·의존성 변경 없이 HTML의 DOM·콘솔·버튼·렌더링을 확인했다. webapp-testing 스킬의 정적 HTML 검증 흐름 사용.

## 실제 테스트 결과

| 검증 | 결과 | 로그/자료 |
| --- | --- | --- |
| 실제 App 전체 ·8threads | **2529통과·0실패·30ignored**,8.60s | `/tmp/deppy-popup-check-app-20261003.log` |
| 최근6개 팝업 오프스크린 렌더 | **2통과·0실패**,2.59s·6개 PNG 생성/모두 확인 | `/tmp/deppy-popup-check-render-20261003.log` |
| 독립 checkout의 추가 동작 probe | **5통과·0실패**,0.36s | `/tmp/deppy-popup-check-extra-probes-20261003.log` |
| HTML Chromium 동작/콘솔 | **25검사 통과**,JS/콘솔 오류0 · 포트 푸터 잘림1건 재현 | `target/popup-parity/20261003-check/html-interactions.json` |
| HTML 여백 실측 | 5개 모두 본문 padding18 + 첫 notice margin16 =34px;33–35 빈 subtitle margin7px | 같은 폴더 `html-spacing.json` |
| 제품 소스 불변·diff | 릴리스 기록 hash 일치·`git diff --check` 통과 | 이번 도구 실행 결과 |

전체 App 테스트에는 최근9개 집중 회귀도 포함된다. 이번 점검에서 i18n·Clippy·릴리스 빌드는 재실행하지 않았으며 이전 작업의 통과 기록을 이번 테스트 결과로 세지 않았다.

실행 명령:

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-performance
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=8
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo env_ports_popup_render -- --ignored --test-threads=1
cd /private/tmp/deppy-popup-check-review-20261003
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo review_probe -- --test-threads=1
```

추가 probe는 실제 현재 dirty source를 별도 detached checkout에 복사하고 기존 테스트 모듈에만 넣었다. 원본 작업 트리의 제품/테스트 코드는 변경하지 않았다. 독립 checkout은 재현을 위해 남겨 뒀다.

## 동작 확인 항목

- **09 프로젝트 목록 닫기**: 실제 표시 함수의 명시 확인, 취소/X/Esc/바깥 클릭, 기본Enter 무반응을 직접 클릭했다. 같은 이름의 다음 대상에 이전 버튼 포커스를 승계하지 않는 기존 회귀가 통과했다. App의 기존 reducer와 테스트를 확인해 캡처한 ID를 설정의 숨김 목록에 넣는 경로이며 세션 종료 명령을 추가하지 않았음을 확인했다.
- **15 변수 삭제**: 취소/X/Esc/바깥 클릭은 의도 없이 확인/선택 초안을 정리한다. 드롭다운에서 `.env`와 모든 파일을 직접 선택하고 명시적 Delete를 눌러 원래 KEY_0와 선택한 원본을 담은 `DotenvWrite { value: None }`를 확인했다. 기본Enter는 삭제하지 않고, 하위 선택 메뉴의 첫Esc는 메뉴만 닫고 다음Esc가 부모를 취소하는 기존 회귀도 통과했다. 새로운 대상이 이전 Delete 포커스를 승계하지 않는다.
- **33–35 안내**: 공용 표시 함수의 X/닫기/바깥 클릭과 기존 Esc·좁은 화면 회귀가 통과했다. App은 세 안내를 if/else 순서로 하나씩 표시하고 대기 플래그를 배경 입력 fence에 포함한다. 전체 App 회귀는 기존 모달·전역 키·터미널 입력 차단도 포함한다.
- **37 포트 목록**: 갱신 의도, 닫기, 재열기 단일 갱신, 정확한 IPv6 주소 복사, 외부/보호 프로세스의 읽기 전용, 기존12번 종료 대상과 확인 유지가 전체 테스트에서 통과했다. 추가 실제 Popup 하네스에서 **280×340pt의 위/아래 앵커** 모두 Refresh/Close 버튼의 전체 rect가 화면 안에 있는지 확인했다. 팝오버가 모달 fence를 만들지 않는 기존 회귀도 통과했다.
- **HTML**: 여섯 화면의 Esc/X 닫기, 원본 `.env.local` 선택과 Enter 무삭제, 두 번째 포트 행의 `[::1]:4000`/Design을12번에 전달, 취소 후37번 복귀를 브라우저에서 확인했다. HTML의 복사/삭제/새로고침은 미리보기용 toast이며 실제 파일/프로세스 동작 검증으로 세지 않았다.

## Details

### 1. HTML 포트 푸터 잘림

1440×1000 브라우저에서 `.preview`는 y60–690(높이630px), 포트 팝오버는 y138–851.70(높이713.70px)이다. `.preview { overflow:hidden }`와 `.overlay.popover-mode`의 위78px 여백이 있는 반면 `.modal`의 max-height는 부모가 아닌 전체100vh를 기준으로 계산된다. 푸터는 y791.70–850.70으로 컨테이너 하단보다 완전히 아래다. 푸터 중앙 `elementFromPoint`는 인벤토리 표를 가리키며 푸터는 보이지 않는다. 버튼 높이34px 자체의 오류가 아니다.

[HTML 전체 스크린샷](../../target/popup-parity/20261003-check/html-37-full.png), [실제 포트 렌더링](../../target/popup-parity/20261003-check/native-37-ports.png).

앱에서는 본문 스크롤과 고정 푸터가 노출되고, 추가280×340pt 위/아래 앵커 테스트도 통과했다. **시안의 잘림을 앱 포트 목록의 잘림으로 보고하지 않는다.** 권장 수정은 `.overlay`의 사용 가능한 높이를 기준으로 `.modal`을 제한하고 flex body에 min-height:0을 주는 것이다.

### 2. 첫 안내·빈 설명 여백 불일치

HTML의 `.notice { margin-top:16px }`가 첫 notice에도 적용된다. 이미 본문 top padding18px이 있어 시작 간격이34px로 실측됐다. Rust `popup::body`는18pt 본문 top padding 후 첫 notice를 바로 그린다.33–35는 내용이 빈 `<p>`를 계속 렌더링해 추가7px의 margin이 생기지만 실제 header는 subtitle이 없으면 그 행을 만들지 않는다. 따라서 안내창의 여백이 같은 구성으로 보이지 않는다. 글꼴 렌더러/테두리의1–2px 차이와 구분되는 명시적 추가 여백이다.

[HTML09](../../target/popup-parity/20261003-check/html-9.png), [실제09](../../target/popup-parity/20261003-check/native-09.png), [HTML33](../../target/popup-parity/20261003-check/html-33.png), [실제33](../../target/popup-parity/20261003-check/native-33.png).

입력36pt/버튼34pt/닫기30pt, 제목18pt, 공용 색상·버튼 순서·고정 머리글/푸터 구성은 코드/측정/렌더링에서 확인했다. 단일 내용으로 정확한 픽셀 동일성을 주장하지 않으며 폰트 fallback과 브라우저의 focus outline 차이는 위 재현된 layout 결함과 구분했다.
