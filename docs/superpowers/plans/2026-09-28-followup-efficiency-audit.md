# 후속 연결 속도·전체 코드 효율 조사 및 PR 개발

사용자 승인: 앞선 작업 완료 후 웹 조사 + 전체 처리/메모리 검토 + 확인된 개선 즉시 PR 개발. 기존 실행 중 앱 유지, 재실행/운영 서버 구축/merge 없음. 순차 작업, 다른 에이전트 요청 없음.

## 조사 범위

27개 workspace crate의 생산 Rust 코드/리소스 경계, 특히 프레임·세션·터널·파일 트리·출력/DB·원격 전송/렌더. 이미 완료한 7개 개선 재구현하지 않는다. 정적 스캔으로 누수 부재를 증명했다고 주장하지 않는다. 읽기 전용 현재 앱 자원 계측은 가능하나 새로운 GUI/벤치 세션 생성은 하지 않는다.

웹 근거: 공식 Quick Tunnel(임시 DNS·SLA없음), pinnedureq pool, eguiMemory clone 및 ScrollArea virtualization, RustPerformanceBook bufferreuse, SQLite partialindex/queryplan. 실제 저장소/측정 결과와 비교한다.

## PR1 — 터널 대기

- [x] 실제 워커 loop/소유 child/cancel baseline측정 + RED 회귀. 정상 연결 250ms/검증50ms sleep, address/등록은1초probe 주기에 묶인 대기.
- [x] 용량1 신호 채널로 최초 주소/등록/EOF/cancel에서 즉시 깨우고 다음 probe/deadline까지 기다린다. Ready public probe10초, verifying1초, TLS/DNS/소유childreap/90초deadline보존. child 조기stderr닫힘에도 bounded fallback status check유지.
- [x] worker wake/cancel전후, 주소/등록과 공개실측3회, cloud회귀/실제publicPTY/ownanswer. SourceCLI리뷰.

## PR2 — MCP 화면 변환 임시 메모리

- [x] 실제 terminal backend snapshot300x80 ASCII/한글/희소grapheme/큰화면 fixture, counting allocator전후.
- [x] 행 임시String을 호출 내 재사용, 제한된 사전예약; 전체문자/공백trim/스페이서/최대크기기존계약보존. 장수 raw cache나redaction우회없음.
- [x] 원본 reference와출력바이트동일, 경계/한글/성능회귀. SourceCLI리뷰.

## PR3 — 감사 이력 정리

- [x] 많은input tombstone +100답변fixture에서실제SQLite finish/recent query VMsteps/시간/DB크기 측정.
- [x] 확인된fullscan에만partialindex추가, query동작/원자성/순서/재시도/100답변/100k중복방지한도보존. migration기존DB검증.
- [x] 이력전체suite/규모별queryplan·시간전후, SourceCLI리뷰.

## 완료

- [x] 기반branch/3개stackPR push/create, 각PR sourcecommit+검증본문. 기존대형미공개changes를PR별diff에중복시키지않고정확한base사용.
- [x] 버전0.2.1→0.2.2 release+별도signedbundle검증, noRestart. 최종report/실측원본/범위한계/옵시디언/양쪽handoff.

## 완료

PR199/200/201 ready/pushed, sources76b326df/f67e1290/6cabc21b.0.2.2 local signedbundleverifiednoRestart. Final cloud32/mcp27passed, public16.22s. DNS tail64.683s remains; GitHubjobsnotstartedbillinglimit. Wholeinventory26sourcecrates, actualtargetedbenchmarks; notwholeappRSSproof. [Report](../../reviews/2026-09-28-followup-efficiency-audit.md).
