# 메모 편집·선택 및 상단 메뉴 수정 —0.6.1

## 적용
- 메모 드래그 선택 후 우클릭: 잘라내기, 복사, 붙여넣기, 삭제, 전체 선택. 선택이 없으면 복사·잘라내기·삭제 비활성.
- 더블클릭은 네이티브 단어 선택을 사용하며 이전 시작점을 포함하지 않는 실제 제스처를 검증. 우클릭 메뉴에서도 선택 범위 유지.
- 상단 전체 메뉴38→36pt. macOS 창 버튼 높이는 같은 상수 사용. 세션 탭 헤더 높이는 변경하지 않음.
- 기존 TextEdit의 실행 취소, 한글/이모지 문자 범위, Edited 자동 저장 경로 재사용. 워크스페이스 변경 시 대기 중 메뉴 편집 취소.

## 실제 검증
- 최종38개 통과/0실패/0무시: 메모22, 기존 타이틀바6, 기존 상단 메뉴2, i18n8.
- Strict App/i18n all-target Clippy, UI capability boundary, workspace fmt, git diff 검사 통과.
- 실제 offscreen egui 드래그·우클릭·더블클릭, 복사 출력, 잘라내기·삭제 후 실행 취소 및 저장 액션, 전체 선택·네이티브 붙여넣기 이벤트, 한글·이모지, 워크스페이스 전환 검증.
- 코드 리뷰6.1-sol/xhigh에서 메뉴 중 Enter가 메모를 바꾸는 Medium1건 발견. 실제 RED 재현 후 수정. 동일 프레임 우클릭+Enter도 RED→GREEN. 메뉴 중 Text/Paste/날짜 단축키의 편집 방지와 Escape 닫기 검증.
- 독립 후속 리뷰: 남은 확인된 지적 없음. 실제 사용자 클립보드나 네이티브 앱 UI는 사용하지 않음.
- 최종 로그: `/tmp/deppy-notes-final-corrected-gates-20261005.log`; 리뷰 `/private/tmp/deppy-notes-20261005/followup-result.txt`.

## 버전·재빌드
-0.6.0→**0.6.1**, 소스 커밋 `8ebcf3b76c981f4018fe378c29cdc6aa558dfe70`. Cargo의 상속된27개 워크스페이스 버전만 갱신.
- 앱: `target/bundle-0.6.1/Deppy Sijo.app`; ZIP: `target/bundle-0.6.1/Deppy Sijo.zip`.
- 최종 gated offline/locked release build 및 macOS 패키지 검증 exit0. 컴파일 버전/내장 버전 문자열/네이티브 About의 CARGO_PKG_VERSION 구성과 plist 두 버전값0.6.1 확인. About UI는 열지 않음.
- 로컬 개발 정책의 서명/아키텍처/ZIP 동일성 검증 완료. 공증된 외부 배포라고 주장하지 않음.
- 바이너리 SHA256: `c475f2221d65a6d3ae3c128c458d642fa238495e30d9acb5bca5c1f18eac5825`.
- ZIP SHA256: `0be2a7426ecca662b64f7bf5f4045e0db471bd0782d582fa306bb01cd2aa8d45`.
- 패키지 로그: `/tmp/deppy-notes-package-final-20261005.log`; 상세 증거 `/private/tmp/deppy-notes-20261005/release-final-proof.json`.
- 리뷰 수정 전 후보는 `target/review-build-0.6.1-notes-unreleased-20261005/`에 보관했으며 배포·실행하지 않음.
- **재실행하지 않음.** 기존 PID21631,0.5.3 실행 파일/ZIP 해시 변경 없음. 이번 작업에서 푸시하지 않음.
