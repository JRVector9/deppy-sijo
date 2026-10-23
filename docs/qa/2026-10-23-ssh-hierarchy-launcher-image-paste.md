# SSH 폴더 트리·에이전트 탐지·이미지 붙여넣기 검증

작업 트리: `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`  
브랜치: `fix/product-review-integration-20261023`  
상태: 세 기능 구현·코드 검토·최종 검증 완료. 실행 중인0.16.0 앱과 서버 사용자 세션은 변경하지 않았다.

## 범위와 원인

- 승인된 [폴더 트리 시안](../mockups/ssh-folder-hierarchy-20261023.html)을 실제 SSH 추가 창에 적용한다. 폴더 펼치기와 선택을 분리하고, 상위 경로 이동·직접 경로 입력·내부 스크롤·전체 선택 경로를 제공한다. 최신 generation/request/path 검증과 서버의 실제 루트 확인은 유지한다.
- `ubuntu@gpu3090`의 비대화형 SSH PATH에는 `~/.local/bin`이 없다. 설치된 Codex와 Grok는 그 경로에 연결되어 있었다. 탐지와 supervisor 실행에 동일한 제한된 설치 경로 목록을 사용한다. 셸 초기화 파일은 실행하지 않는다. 서버 카탈로그도 런처와 같은12종으로 맞췄다.
- 원격 터미널의 클립보드 요청은 로컬 파일 작업으로 일괄 거부되고 있었다. 이미지 바이트를 서버에 올린 뒤 서버 경로를 붙여넣도록 변경한다. Mac 경로를 SSH 터미널에 보내지 않는다.

## 승인 시안과 실제 구현 대조

| 항목 | 실제 구현·확인 |
|---|---|
| 상위·하위 계층 | 실제 서버 조회 결과로 지연 로딩, 폴더 화살표와 이름 선택 분리 |
| 경로 막대 | 상위 이동, 클릭 가능한 breadcrumb, 명시적 직접 입력,36pt |
| 목록 행 | 공통34pt, 선택·hover 공통 팔레트, 글꼴에 의존하지 않는 아이콘 |
| 스크롤·하단 정보 | 목록 내부 스크롤, 선택 전체 경로 줄바꿈, 고정 추가/취소 버튼 |
| 연결 후 공간 | 연결된 서버 한 줄 요약, 연결 상세·선택 이름 기본 접힘 |
| 키보드 | 방향키 펼침/초점, Home/End, Enter/Space 선택, 경로 편집Esc |
| 비동기 응답 | 현재 generation/request/path만 적용, 펼치기·탐색으로 검증된 선택 변경 없음 |
| 좁은 화면 | native800×950/390×700 렌더에서 전체 선택 경로 실제 paint와 footer 위치 확인 |

네이티브8상태×2화면 크기의 렌더 테스트를 실행했다. 좁은 렌더 fixture는 선택을 코드로 지정하므로 트리의 시작 scroll 위치는 상위 폴더다. 실제 클릭 선택은 보이는 행에 키보드 초점을 주며, 방향키 이동은 한 번만 스크롤한다. 전체 선택 경로는 목록 밖에 계속 표시한다.

## 입력 계약

- 로컬 worker에서 최대16개/합계32MiB 이미지를 읽고,256KiB씩 인증된 SSH RPC로 전송한다. 서버는0700 캐시/0600 파일에 저장하고 SHA256 확인 후 원자적으로 공개한다. 캐시 상한64개/256MiB/24시간.
- 응답의 workspace·upload UUID·확장자·완료 오프셋을 검증한 후에만 다음 단계로 이동한다. 모든 이미지가 완료된 뒤 하나의 붙여넣기를 보낸다. 원래 세션/operation/generation/runtime/SSH 대상과 연결 응답 fence를 유지한다.
- 텍스트 붙여넣기는 기존 경로를 사용한다. 이미지 완료가 Enter를 보내지는 않는다. 하단 composer의 첨부 선택·드래그 UI까지 지원한다고 주장하지 않는다.

## 실행한 검증

공통 Cargo 환경: `CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`. 실행 로그는 `/tmp/deppy-ssh-20261023-qa/`에 있다.

| 검증 | 실제 결과 |
|---|---|
| HTML 클릭·탐색·선택·키보드 | 10개 확인 통과,1180px/390px 실제 viewport 확인 |
| 원격 서버 단위 테스트 | macOS29통과/2별도 child fixture, Linux ARM64 musl29통과/2별도 child fixture |
| 실제 helper stdio·supervisor·PTY | macOS4통과, Debian12 Linux ARM64 비특권 사용자4통과 |
| 이미지 클라이언트 | RED6통과/2실패 → 수정 후10통과 |
| 이미지 캐시 | 추가 회귀3실패 →8통과, fork 잠금 회귀 RED→GREEN |
| 서버 all-target Clippy | PATH 부등식 경고 수정 후 통과 |
| 네이티브 트리·실제 키보드·경로 paint | 21통과/0실패/1별도 렌더, 전체 App에서도 통과 |
| 전체 App 단위·통합 | 3721단위+39통합 통과/0실패,49개 별도 수동·계측 항목 제외 |
| 앱·서버·공통 popup all-target Clippy | 최종 통과 (`-D warnings`) |
| 전체 포맷·구조 경계·다국어 | 모두 통과; 다국어1752리터럴 키 호출/5개 locale |
| 최종 네이티브8상태×2viewport 렌더 | 16PNG 생성 통과, desktop/narrow 직접 대조 |

이미지 전송 실패, FIFO·심볼릭 링크·하드 링크, 용량 초과, 잘못된 완료 경로, 중복 완료, 대상 변경을 포함한다. 실제 서비스 비용이 발생하는 에이전트 작업은 실행하지 않았다.

## 발견 후 바로잡은 검증 조건

- `flock`을 close만 하면 concurrent fork의 상속 descriptor가 잠금을 유지했다. 명시적 RAII unlock과 pipe barrier 회귀 테스트로 확인했다.
- Linux 통합 테스트의 임시 폴더가 Docker 기본 `noexec`여서 fixture가 실행되지 않았다. PTY의 실제 PATH/HOME와 `/proc/mounts`를 확인하고 테스트 mount에만 `exec`를 추가했다. 제품 PATH 우회 처리는 추가하지 않았다.
- 첫 native 렌더에서 선택 경로가 아래로 밀리고 폴더 아이콘이 깨지는 것을 직접 확인했다. 연결 정보 축약과 painter 아이콘으로 수정했다. 이어서 무제한 높이를 쓰던 RTL 연결 요약을34pt로 제한하여 트리와 전체 경로가 실제 화면에 표시되는 것을 확인했다.

- 실제 키보드 회귀에서 클릭 후 초점 누락, egui 기본 방향키와 중복 이동, 경로 편집Esc의 외부 모달 닫힘, focus 필터의 Context lock 재진입을 발견했다. 수정 후 방향키·Home/End·Enter/Space·Esc를 포함한21개 트리 테스트가 모두 통과했다.
- 첫 전체 App 실행은3720통과/1실패/46제외였다. 실패는 기존 프로젝트 키 테스트의 시작 직후 `select_scope().unwrap()`이 의도된 비차단Busy를 처리하지 않은 경쟁 조건이다. 동일 소스 단독 실행은 통과했고 제품 UI에는 이미 재시도가 있었다. 이 테스트의 첫 선택에만5초 이내Busy 재시도를 추가했고 다른 오류·후속 검증은 유지했다.
- 첫 통합Clippy는 이미지 오류 fallback의 소유자200바이트 때문에 실패했다. 해당 fallback만Box로 보관하도록 수정했고 lint를 끄지 않았다. 수정 후 전체 App와 통합Clippy·포맷·렌더를 다시 실행해 모두 통과했다.

## 실제 구현 화면과 증거

[네이티브 일반 화면](assets/ssh-hierarchy-20261023/native-desktop.png) · [네이티브 좁은 화면](assets/ssh-hierarchy-20261023/native-narrow.png) · [소스·로그 SHA256 목록](assets/ssh-hierarchy-20261023/evidence.json)

위 디렉터리에 최종 검사 로그와 실패→수정 증거를 `.log.gz`로 보존했다. 반복 실행과 child fixture 결과는 전체 통과 개수에 중복 합산하지 않았다. 버전0.16.0은 기존 출시 버전이며 이 변경의 새 출시 버전으로 주장하지 않는다.

## 참고와 검증 한계

[OpenCode 공식 설치 스크립트](https://opencode.ai/install)의`.opencode/bin` 설치 위치도 탐지 목록에 포함했다. 원격 이미지 전달 방식은 [VS Code Claude Image Paste의 구현](https://github.com/benjaminwood/vscode-claude-image-paste)과 [Claude Code 원격 클립보드 이슈](https://github.com/anthropics/claude-code/issues/48402)를 참고했다. 개별 CLI의 모든 버전에서 이미지 표시가 동일하다고 보장하는 근거로 사용하지 않았다.

새 앱·Linux helper를 패키징하거나 실행 서버에 설치하지 않았다. 변경된 제품을 배포할 때는 현재 출시 버전보다 높은 버전과 일치하는 helper 빌드가 필요하다. 현재 실행 앱에서의 사용자 SSH 세션 이미지 E2E는 아직 수행하지 않았다.
