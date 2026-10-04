# 9개 PR 병렬 구현 체크리스트 — 진행 중

기준:0.5.5현재소스,2026-10-03전체감사. 사용자2026-10-04병렬구현승인. 아래 체크는 실제 코드 통합/테스트/리뷰가 끝났을 때만 완료로 바꾼다.

| PR | 작업 | 담당 | 상태 | 코드·테스트·최종 리뷰 |
|---|---|---|---|---|
| 1 | 입력 승인·초안/예약 복구 | audit_pr1 | 코드·리뷰 완료 | 새 소스 테스트·Clippy 및 독립 리뷰 완료 |
| 2 | AI 브로드캐스트 자격·초안 guard | audit_pr1 | 코드·리뷰 완료 | 실제 fallback·draft·dialog 회귀 및 독립 리뷰 수정 완료 |
| 3 | 라이브러리 복구·비동기 저장 | audit_pr3 | 코드·리뷰 완료 | 새 소스 테스트 및 생성 ID/복구 리뷰 수정 완료 |
| 4 | 검색 캐시·가상 행·초안 복사 제거 | audit_pr3 + root | 코드·리뷰 완료 |14회귀·Fleet26/43 통과; 예약 보관 수정·독립 리뷰 완료 |
| 5 | 세션 초안·저장·메모리 예산 | audit_pr3 | 코드·리뷰 완료 |29회귀·Composer72 및 독립 리뷰 완료; 독립 리뷰 수정 완료 |
| 6 | 이미지 제한 읽기·worker·예산·재사용 | fix_performance_review | 코드·리뷰 완료 | 새 소스 테스트·Clippy 및 독립 리뷰 완료 |
| 7 | 클라우드 이력 DB worker | fix_performance_review | 코드·리뷰 완료 | 실제 DB 잠금·권한 회귀 및 독립 리뷰 완료 |
| 8 | 배치 wake·로그 길이/배치 계측 | fix_performance_review | 코드·리뷰 완료 |Storage408/Runtime324 및 실측·부분쓰기 cap 수정 통과; 독립 리뷰 수정 완료 |
| 9 | 공용 전송 기반 remote paste | audit_pr9 | 코드·리뷰 완료 | MCP30/cloud54/App7/Runtime+PTY 회귀 통과; 독립 소스 리뷰 확인된 버그 없음 |

## 공통 확인

- [x] 기존 dirty소스 보존·별도 작업트리 격리
- [x] 설계/파일범위/회귀 조건 문서화
- [x] 변경 전 allocation/time benchmark 실행
- [x] 각 PR observedRED/GREEN 기록
- [x] 순차 코드 통합·중복 구현/권한/정리 검토
- [x] 9개 PR 독립 Codex CLI 소스 리뷰·지적 수정
- [x] 실제 영향 package/fullApp/runtime/PTY/storage/MCP 테스트 — 최종 통합4,824/0/47기존제외 및 strict gates 통과
- [x] 변경 후 의미 있는 성능/메모리/응답성 측정 — 실제 모듈5회중앙값/privatePTY/64개파일효과, App RSS/FPS 아님
- [ ] 최종 버전 증가·재빌드·bundle/compiled버전 검증
- [ ] 최종 결과·제한·체크리스트 보고

변경 전5sample median(Systemallocator함수harness,실제AppRSS/frame아님):1MiBclone12.514us/1048575B;1000×8000Bsearchmiss16.609ms/8015901B;1000save3.584ms/16548173Bcumulativealloc. `/tmp/deppy-nine-pr-before-bench-20261004.log`.

## 후속 승인 작업

9개 PR 및 리뷰 수정 완료 후 새 소스 `9c7af797a4db4786b0dcc1187980e7ba71ead1de`에서 아래 두 작업을 시작했다. 최종 산출물 gate/버전 검증은 후속 통합 후 한 번 더 실행한다.

- [x] 9개 PR 완료 후 Grok AI 터미널 **직접 입력** 지연 원인·측정·개선 검토 — PR10 첫 pass 전송/Busy보존 및 실제 private한글echo; nativeGrok미측정
- [x] Shift+마우스 드래그 다중 선택 후 전체 복사·삭제·이동 회귀 수정 — PR11+11r64개효과/nativebackend/실제볼륨충돌; 마지막corrective독립리뷰진행중

검증 주의: 공용target의Cargo 자체 잠금은 test실행 전 해제되어 다른 worktree binary로 바뀔 수 있다. 이전 병렬 fullApp 결과는 최종 통과 근거에서 제외한다. `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`는 compile+execution 전체를 잠그고 작업트리 전환 시 workspace artifact를 정리하여 source fingerprint 재사용도 차단한다. 이전 gate 결과도 고유 테스트 이름/실행 소스를 다시 확인한 결과로 대체한다. 최종 테스트는 통합 소스에서 실행한다.
