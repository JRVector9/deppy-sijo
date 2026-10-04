# 9개 PR 병렬 구현 체크리스트 — 완료

시작: 2026-10-04. 완료: 2026-10-05(KST). 기준은 기존0.5.5 전체 소스와 2026-10-03 감사다. 실제 변경·테스트·리뷰가 완료된 항목만 체크했다.

| PR | 작업 | 담당 | 결과 |
|---|---|---|---|
|1|원자적 입력·정확한 승인·초안/예약 복구|audit_pr1|[x] 구현·RED/GREEN·독립 리뷰 수정 완료|
|2|자동 입력 AI 자격·실제 초안/dialog guard|audit_pr1|[x] fallback·프로세스·초안 회귀 완료|
|3|라이브러리 복구·비동기 저장|audit_pr3|[x] 손상/빈 파일·저장 순서·새 폼 복구 완료|
|4|검색 캐시·가상 행·본문 복사 제거|audit_pr3 + root|[x] palette/Fleet 예산·undo·거절 후 폼 회귀 완료|
|5|세션 초안 복구·저장 확인·메모리 예산|audit_pr3|[x] 세션 분리·종료 정책·Pending 복구·잔류 용량 측정 완료|
|6|이미지 읽기 제한·worker·재사용|fix_performance_review|[x] 실제 성장 파일·generation·픽셀/텍스처 회귀 완료|
|7|클라우드 이력 DB worker|fix_performance_review|[x] 실제 DB 잠금·claim/finish·권한/원래 세션 회귀 완료|
|8|배치 wake·로그 길이/쓰기 효율|fix_performance_review|[x] 실제 로그 측정·부분쓰기 cap 복구 완료|
|9|공용 전송 기반 MCP paste/답변|audit_pr9|[x] 실제 MCP/privatePTY·DEC2004·원래 권한/세션 회귀 완료|

## 공통 확인

- [x] 기존 dirty 소스·실제 HEAD/index 보존, 작업트리 격리 및 충돌 관리
- [x] 설계/파일 범위/회귀 조건 문서화
- [x] 각 PR의 실제 RED/GREEN 및 실패한 접근 기록
- [x] root 실제 소스 확인·통합, 별도 Codex CLI 리뷰와 모든 확인된 지적 수정
- [x] 최종 소스 전체 테스트: **4,826 passed /0 failed /47 기존 ignored**
- [x] strict workspace all-target Clippy, fmt, diff, UI boundary,27-crate dependency gate
- [x] 변경 전후 실제 모듈5회 중앙값/privatePTY/64파일 효과 측정
- [x]0.5.5 → **0.6.0**,27 workspace/lock 항목·컴파일 버전·두 bundle version 필드 확인
- [x] 재빌드, 앱/helper 서명·architecture·ZIP 내용 hash 검증
- [x] PR별 커밋·로컬 통합 브랜치·Obsidian 일지·결과 보고서 작성
- [x] **앱 종료·실행·재실행 없음**, 기존 실행 파일/ZIP hash·PID 보존 확인

## 후속 승인 작업

- [x] AI 터미널 **안에서 직접 입력**하는 경로: 다음 logic frame 대기 제거, bounded FIFO/known-unsent Busy 보존, 한글 private echo·16ms repaint 확인. Composer 경로와 구분했다.
- [x] Shift+마우스 드래그 다중 선택: frozen 선택 전체를 공용 copy/move/delete 엔진으로 전달,64개 실제 효과 확인.
- [x] 실제 macOS native `⌘⌥V` backend 이벤트·다른 입력 소유자/반복·Dvorak 키 배열 확인.
- [x] 목적지 실제 case/Unicode 이름 충돌·임시 probe 정리·case-sensitive 테스트 가정 보완.
- [x] Runtime fixture의 native 키체인 대기 제거; 제품 resolver 변경 없이 실제 PTY redaction 검증 유지.

## 검증 범위와 근거

최종 freeze `d1818e3355e9604998e6581285bad8e7edd88ffb`, 동일 제품 소스 커밋 `c532ce0ad2c5edcc9e3dcbc779a61b37d5ea53a2`. 마지막 작은 소스 CLI: 확인된 남은 지적 없음. 실제 명령·before/after 수치·개별 PR 보고서·제한은 [최종 결과 보고서](2026-10-04-final-improvements-report.md)에 정리했다.

측정은 실제 소스/allocator/privatePTY/파일 작업이다. native Grok 화면 지연이나 App 전체 RSS/GPU/FPS의 개선 수치를 주장하지 않는다. 기존 공유 target의 잘못된 worktree binary 결과는 최종 근거에서 제외했고, compile+execution 전체 잠금과 소스 전환 정리를 적용한 coherent root 결과를 사용했다. GitHub CI/push나 공증된 공개 배포는 수행하지 않았다.
