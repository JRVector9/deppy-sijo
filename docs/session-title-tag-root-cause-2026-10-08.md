# 세션 제목 태그 재발 원인과 수정 — 2026-10-08

## 확인한 원인

Claude native JSONL의 `type: user`는 실제 사용자 지시뿐 아니라 내부 알림, 에이전트 전달문, 붙여넣기 포장도 담는다. 원래 제목 경로는 내부 접두사를 종류별로 제외했지만, **본문을 제목용 텍스트로 투영하는 단계 없이 원문을 먼저 요약**했다. `<task-notification>`과 `<agent-message>` 필터를 추가해도 사용자 지시를 감싼 `<pasted_content>`는 유효한 user 메시지로 통과했다. 화면의 문자열을 잘라 표시하기 전에 태그와 id까지 요약 예산을 소비했기 때문에 다시 노출됐다.

읽기 전용으로 실행 중 앱의 metadata DB와 등록된 native 기록을 조사했다. 스크린샷의 `e89a` 이벤트는 nomorevibe Claude 기록의 2026-10-08T00:54:10.334Z user 메시지이며 664바이트이고 닫는 태그가 없다. Hook/agent의 저장된 task_prompt에서 이 태그가 나온 행은 0건이었다. 이번 화면의 직접적인 경로는 native transcript → clean_agent_summary → AgentDisplay → 세션 행이다. 기존 hook 제목 저장은 256바이트 제한을 유지하므로 긴 원문을 저장한 뒤 오염된 경우로 추정하지 않았다.

추가로 등록된 Claude native 파일 5개를 각각 끝 2MiB 범위에서 조사했다. user 텍스트의 첫 태그 집계는 task-notification 17, local-command-caveat 3, command-name 3, local-command-stdout 3, pasted_content 1이었다. 이 범위에서 관찰된 메타데이터 종류는 모두 공통 판정/투영이 처리한다. 이 집계는 전체 과거 기록을 전수 검사한 결과가 아니다.

## 수정한 경로

`crates/storage/src/task_prompt.rs::task_prompt_text`를 공통 제목 투영 지점으로 만든다.

- 실제 지시를 감싼 알려진 leading pasted_content/image 메타데이터를 먼저 제거한다. 따옴표 안의 `>`를 종료로 오인하지 않는다.
- 닫는 태그가 없는 native 이벤트를 처리한다. 닫힌 블록 뒤의 추가 지시도 보존한다. 중첩/잘못된 메타데이터에는 깊이 8과 opening 512바이트 한도를 적용한다.
- 포장을 제거한 뒤 내부 알림/전달문을 판정한다. 내부 메시지는 새 작업 제목을 만들거나 기존 pane 작업을 덮지 않는다.
- 일반 HTML, 유사한 태그 이름, 코드 블록은 기존 작업 내용으로 유지한다. 임의의 XML을 일괄 삭제하지 않는다.
- 보통 경로는 원문을 빌리고, 중간 closing marker를 제거할 때만 문자열을 만든다.

같은 함수를 native 사용자 지시/assistant 요약/최근 턴, hook 제목 저장, fresh pane 제목, 이미 저장된 제목 복원, shutdown/binding 저장 경로에 연결했다. Hook의 256바이트 제목 예산은 메타데이터를 제거한 본문에 적용한다. pane/native id 및 fork 복원 검증은 유지한다. 공유 세션의 소유권 비교는 정규화한 정확한 지시가 유일할 때만 성공하며, 같은 본문을 가진 다른 pane이 있으면 계속 모호한 것으로 처리한다.

세션 행·Fleet·PWA는 이 정규화된 AgentDisplay를 소비한다. 원문 대화 뷰어는 제목 생성 경로가 아니므로 원문을 유지한다. 실제 PTY 입력, native JSONL, 실행 중 앱의 DB를 변경하지 않는다.

## 검증

수정 전 native 파서/새 제목/저장 제목/hook 저장 회귀가 실패했다. 처음 borrowed suffix-only 구현은 닫힌 블록 뒤에 추가 지시가 있을 때 middle closing marker를 남겨 추가 회귀가 실패했다. Cow 투영으로 본문과 후속 지시를 모두 보존하도록 고쳤다.

세 종류의 재발 사례와 포장된 내부 메시지를 native string/array 입력, 완료 상태 보존, fresh/restored pane hydration, hook 예산 및 기존 작업 보존에서 검증했다. `/private/tmp/deppy-paste-three-incidents2-20261008.log` exit0: storage 4 PASS, App 4 PASS/1 ignored, 명시적으로 실행한 실제 native 파일 검사 1 PASS. 실제 파일 검사도 수정된 제품 파서로 문제의 pasted 이벤트와 현재/최근 제목을 확인했으며 비공개 프롬프트는 출력하지 않았다.

최종 전체 게이트 `/private/tmp/deppy-paste-final-20261008.log` exit0: Storage 420 PASS, App 2806 PASS/38 ignored, integration 4+5+15+15 PASS(플랫폼/리소스 검사 3 ignored), Proxy 81 PASS/1 ignored, i18n 8 PASS. App/Storage/Proxy/i18n all-target strict Clippy, fmt, xtask boundary 모두 PASS. 이후 Rust 소스를 변경하지 않았다. 앱 재실행·제품 배포·버전 변경은 이번 요청에서 실행하지 않는다. 실행 중 0.8.7에는 이 소스 변경이 아직 반영되지 않았다.
