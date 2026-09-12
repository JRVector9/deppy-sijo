# 환경 및 API 재리뷰와 수정 결과

대상: 1dd86ba 대 96cd1ff의 Rust 변경 및 실제 호출 경로. 실제 Codex CLI 소스 리뷰와 자체 점검을 함께 수행했다. 운영 DB/사용자 설정을 읽거나 변경하지 않았고 실행 앱을 종료하지 않았다. 테스트는 합성 상태만 사용했다. 재빌드는 승인됐고 재실행은 범위 밖이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/app/src/app.rs · settings_environment_entry | 열린 설정 창으로 재진입할 때 이전 프로젝트 유지 | 현재 화면과 대상 불일치 | 수정·회귀 통과 |
| medium | crates/app/src/ui/environment.rs · current_cwd | 재활성화 runtime의 오래된 폴더 경로 사용 | 잘못된 폴더 등록 안내 | 수정·회귀 통과 |
| medium | crates/app/src/app.rs · set_settings_category | 외부 카테고리 전환에서 prefill 미정리 | 이전 비밀값 재등장 | 수정·호출 경로 확인 |
| medium | crates/app/src/ui/credentials/modern.rs · reset_modern_draft | API 이름의 실행 취소 기록 잔류 | 프로젝트 간 이름 혼입 | 수정·회귀 통과 |
| low | crates/app/src/ui/environment.rs · editor | 변수→API 전환 때 기본 서비스 초기화 누락 | 표시와 입력 가능 상태 불일치 | 수정·화면 확인 대기 |

## 수정 근거

- 명시적 설정 진입을 프레임 유지와 구분한다. 모든 6개 진입은 request_settings_open을 거치며 현재 프로젝트가 다를 때만 다시 선택한다. 같은 프로젝트에서 기존 창을 앞에 띄우는 경우에는 초안을 유지한다. 기존 본문으로 정상 기대 assertion이 실패한 뒤 수정 후 통과했다.
- 원본 pane의 세션 ID와 캡처 cwd를 유지하되, primary 전환 때 초기화되는 App 감지 캐시와 일치해야 폴더 안내에 쓴다. 캐시가 비었거나 다른 경로로 갱신됐으면 생략한다. 첨부 pane의 폴더를 활성 프로젝트에 등록하는 경로는 허용하지 않는다.
- 벨/홈/단축키/우클릭의 카테고리 변경을 set_settings_category로 모아 Environment를 떠날 때 기존 reset_environment_view_state를 호출한다. 설정 내부 nav의 정리와 동일하게 pending prefill, 공개값, 초안과 입력 기록을 정리한다.
- 이름 필드도 DraftTextEditState로 추적한다. 새/기존 화면에서 ID를 공유하고 저장 요청 및 성공 ACK, 취소, 화면 전환, prefill 교체에서 공유 undo 버퍼와 위젯 상태를 제거한다. 가짜 이전 이름의 상태가 남는 실패를 확인한 뒤 수정했다.
- API editor로 들어갈 때 기존 begin_modern_add를 호출한다. 직접 입력 모드와 prefill은 이 메서드의 기존 보호 조건으로 보존한다.

## 검증 범위

- 관련 상태 테스트 15 PASS: environment_context 10, pr188 3, 목록 refresh 선택 보존 1, 프로젝트 닫기 격리 1. API 이름 테스트는 취소/교체/저장 ACK를 모두 확인한다.
- 외부 카테고리 메뉴 클릭과 화면 배치는 소스 경로만 확인했으며 실제 GUI PASS를 주장하지 않는다.
- 독립 리뷰 로그: /tmp/deppy-env-context-rereview-20260912.log. reviewer는 테스트/빌드/파일 변경을 실행하지 않았다.
- 재현/결과: /tmp/deppy-env-rereview-reentry-behavior-red.log, /tmp/deppy-env-rereview-review3-red.log, /tmp/deppy-env-rereview-final-related.log.
- 최종 gate 로그: /tmp/deppy-env-rereview-final-gates.log. 최종 빌드/서명/실행 상태는 docs/CODEX_HANDOFF.md 마지막 절에 기록한다.
