# 성능·메모리·에이전트 터미널 개선 결과

상태: 통합 검증 진행 중. 배포 완료를 뜻하지 않는다. 현재 실행 중인 앱은 종료하거나 재실행하지 않았다.

## 작업 범위

기존 전체 감사의 9개 PR을 격리된 작업 트리에서 구현하고 root가 통합했다. 변경 전 전체 소스 스냅샷은 `85631a845d733b340e281df976522be4d4468959`이다. 기존 미커밋 작업과 실제 Git index를 보존했다. 후속 작업은 AI 터미널 직접 입력 지연과 파일 트리 다중 선택이다.

| PR | 수정 내용 | 변경 전 실제 증거 | 변경 후 실제 증거 |
|---|---|---|---|
| 1 | 본문·Enter 원자적 입력, 정확한 ACK와 미확정 초안 보존 | 큐 포화·거절·오래된 ACK 회귀가 실패 | 관련 App 10·Composer 57·runtime 3·PTY 4 통과 |
| 2 | 자동 입력 대상 AI와 실제 초안·선택창 확인 | 일반 셸/종료 후 fallback 및 초안 상태 오분류 재현 | 실제 프로세스·초안·대화상자 회귀 통과 |
| 3 | 손상/빈 라이브러리 보호, 순차 비동기 저장 | 기존 파일 복구·저장 순서 회귀가 실패 | 라이브러리 20개 초기 회귀 통과; 이후 확장 회귀 포함 |
| 4 | 검색 캐시, 가상 행, 공용 입력 예산, 본문 복사 제거 | 1,000행 렌더·폼 간 undo 유출·초과 입력 재현 | 표시 행 11개; 캐시/undo/거절 후 폼 보존 회귀 통과 |
| 5 | 세션별 초안 복구, 저장 확인 후 입력, 메모리 예산 | 세션 간 초안/재시작 복구 실패; 비운 버퍼 9,830,400B 유지 | PR5 29·Composer 72 통과; 비운 버퍼 잔류 용량 0B |
| 6 | 이미지 제한 읽기·worker·재사용 | stat 후 증가 파일 8,388,609B 읽기, 50M 픽셀 등록 | 초과 파일 거절; 최대 8M 픽셀; 텍스트 변경 시 동일 Arc/텍스처 유지 |
| 7 | 클라우드 이력 DB 작업 분리 | 실제 SQLite 잠금에서 interactive 호출 129.735ms | handle + 100회 poll 42.709µs; 권한/이력 순서 회귀 통과 |
| 8 | 배치 wake와 로그 길이/쓰기 효율 | 120프레임 본문 복사 1,966,080B; metadata 2,048회 | 본문 복사 0B; 정상 metadata 0회; 로그 배치 512→2회 |
| 9 | MCP 명시적 paste와 원래 세션 답변 수신 | 실제 MCP discovery·paste·DEC2004 변경 회귀 실패 | MCP 30·cloud 54·App 7 및 실제 private PTY 회귀 통과 |

테스트 그룹은 서로 겹친다. 위 숫자를 합산하지 않는다. 각 PR의 실행 명령·RED/GREEN·실패한 접근·측정 범위는 같은 폴더의 개별 보고서에 있다.

## 측정 수치

| 측정 대상 | 이전 | 이후 |
|---|---|---|
| 닫힌 palette의 1MiB 초안 처리 | 19.701µs / 1,048,575B 할당 | 본문 복사·할당 0B |
| 1,000개 검색 miss | 25.690ms | 첫 검색 26.639ms; 동일 쿼리 캐시는 재검색·할당 없음 |
| 1,000개 checked save 누적 할당 | 33,620,288B | 65,917B |
| 1,000개 checked save 시간 | 16.193ms | 14.675ms |
| 100개 checked save 시간 | 5.815ms | 5.802ms: 비슷함 |
| 실제 파일 2,048×128B append | 5.160875ms | 3.435125ms |
| 512개 chunk의 실제 로그 쓰기 | 3.789917ms | 0.615875ms |

검색·저장 수치는 리뷰 수정까지 통합한 실제 root `9c7af797` 모듈과 기존 baseline을 같은 release harness에서 비교한 5회 중앙값이다: `/private/tmp/deppy-core-bench-20261004/src/main.rs`, `/tmp/deppy-core-current-measurements-20261004.log`, gate exit0. 원래 격리 PR 측정도 각 보고서에 보존했다. 첫 검색의 속도 개선은 확인되지 않았다. 동일 쿼리 캐시와 할당/표시 행 감소가 확인된 개선이다. 로그 시간은 PR8의 실제 파일 비교 측정이다.

캐시의 0ns 표시는 타이머 해상도이며 실제 지연이 0이라는 뜻이 아니다. 프로세스 RSS·GPU 메모리·실제 화면 FPS 측정으로 확대 해석하지 않는다. 이미지/초안/예약 예산은 논리적 보관량이며 allocator·undo·worker의 겹친 버전은 별도다.

## 코드 리뷰와 수정

독립 소스 리뷰의 5개 지적은 로그 cap 오류 복구, Composer 보관 예산, 예약 보관 예산, Fleet 입력/undo, Batch UI 제한으로 수정했다. 최종 9개 소스 CLI 리뷰에서 추가로 확인한 3개 지적도 수정·통합했다.

- PR13: 입력 승인 직전 이미 큐에 들어온 선택창/DEC2004 출력을 원래 세션에서 최대 256KiB 최신화한다. backlog가 남으면 본문과 Enter를 모두 미전송 거절한다. 로그/이벤트/최종 화면 처리는 권한 잠금을 푼 뒤 실행한다. 빠른 종료의 마지막 watched 화면 누락도 실제 RED/GREEN으로 수정했다.
- PR14: 예약별 입력 권한을 큐와 공유해 취소·유효한 교체·종료 전에 회수한다. 실패한 교체와 새로운 예약의 권한은 보존한다. 이미 접수된 입력을 되돌리거나 Unknown을 자동 재전송하지 않는다.
- PR15: 긴 제목의 생성 ID와 충돌 suffix를 256B 안으로 제한한다. 새 프롬프트 저장 거절 후에도 새 항목 상태와 본문을 보존한다.

추가 3개 수정의 통합 named gate 및 strict workspace Clippy/fmt가 통과했다. 독립 CLI는 PR13의 종료 PTY destructor가 권한 잠금 안에서 실행되는 추가 경로를 확인했다. 이를 즉시 입력 경로에서 분리하고, 실제 자식 프로세스 종료 handshake RED/GREEN과 별도 소스 CLI로 검증했다. 마지막 CLI는 확인된 도입 버그가 없었다. 관련 최종 runtime 336·Session 75·PTY 45 통과 및 strict Clippy/fmt를 확인했다.

기존 private secret-env fixture의 timeout은 baseline에서도 발생했다. 중간 실패 기록을 보존하고 실패 시 자체 테스트 이벤트/행/저장소 상태를 확인하는 진단만 추가했다. 마지막 전체 runtime gate는 336 passed였으며, timeout 원인을 확인했다고 주장하지 않는다. 최종 coherent gate에서 다시 확인한다.

## 후속 수정 결과

| PR | 변경 전 | 변경 후 실제 검증 |
|---|---|---|
| 10 직접 AI 터미널 입력 | UI에서 만든 입력을 다음 logic pass에서 전송; 큐 포화 시 소유 bytes 유실 | 첫 pass0B/다음6B → 첫 pass6B; 원래 bytes/세션의 미전송 Busy만 제한 재시도; App14·Workspace340 및 runtime/경계/strict 통과 |
| 11 파일트리 선택 전체 작업 | 실제 Shift marquee3개가 드래그에서1개로 줄어듦;16개 파일 한도 | frozen3개 이동/Option복사;64개 전체 복사·이동·삭제;20named·FileTree181 및 영향 회귀/strict 통과 |

PR10은 빠른 연속 입력을 합칠 때 버퍼 확장256회→9회도 확인했다. 실제16ms 재시도가 egui에서 즉시 repaint가 되는 경우를 재현·수정했다:0µs→15,867µs. 한글 IME Commit+Enter가 같은 pass에서 정확한 한 입력으로 private cat까지 전달됐으며, 단일 fixture host356µs/echo3,223µs다. 실제 실행 중인 Grok/provider 화면을 조작하거나 측정한 값은 아니다. 채널 admission과 이후 PTY acceptance는 구분하며 이미 접수된 입력을 무조건 재전송하지 않는다.

PR11은 공통 상한4,096개/경로합계1MiB/개별32KiB, 조상 중복 제거, 전체 충돌·위험 경로 preflight, 원래 tree root/generation과 연속 삭제 재확인, destructive Cmd+Option+V tree 포커스, typed DND를 적용했다. OS clipboard/Trash는 주입한 private provider로만 검증했다. 알려진 preflight 실패는 첫 효과 전 거절하며, 이후 OS 오류에 대한 여러 파일 rollback을 보장하지 않는다.

세부 명령·실패 기록·측정 범위: [직접 입력 PR10](2026-10-04-pr10-direct-input-latency.md), [다중 선택 PR11](2026-10-04-pr11-tree-multiselect.md). 두 clean commit을 실제 root index 변경 없이 통합했다. 통합 제품 소스 freeze는 `f875919b3d0fe9355f51126bba30b29cf6681eda`다.

## 최종 검증과 후속 작업

### 마지막 통합 검증에서 확인한 추가 수정

고정 소스 `f875919b`의 두 후속 PR 독립 CLI는 PR10 지적 없이, PR11에서 아래 두 실제 경로를 확인했다. 수정과 마지막 재검증은 진행 중이다.

- pinned egui-winit가 Cmd+Option+V press를 Paste 또는 무이벤트로 변환하므로, 주입한 pressed-key fixture와 달리 실제 파일 전용 clipboard move는 시작하지 않는다. 실제 native gesture 계약을 수정한다.
- 목적지 디스크의 대소문자/정규화 규칙과 raw OsStr planned-name 비교가 달라, `p/a.txt`·`q/A.txt` 그룹에서 알려진 충돌을 첫 효과 전에 놓친다. 실제 destination volume 규칙으로 preflight를 보완한다.

전체 coherent gate는 Runtime 제외4,475 passed /0 failed /47 existing ignored 후 중단했다. 단독 serial Runtime337은 기존 `로그_secret_scan_평문_없음`에서8분 이상 기다렸다. 테스트 소유 PID76085의 실제 sample은 `SecretStoreResolver→KeyringSecretStore→SecItemCopyMatching` 대기를 확인했다. 기존 keyring mock builder는 의존 crate의 custom macOS 경로를 가로채지 못한다. root는 해당 테스트 프로세스만 종료했고 gate exit101을 보존했다. `cfg(test)` 메모리 저장소를 명시적으로 주입해 이 테스트의 native 키체인 의존을 제거한다. 기존 과거 secret-env timeout의 원인을 모두 증명한 것은 아니다.

근거: `/tmp/deppy-final-followups-cli-result-20261004.txt`, `/tmp/deppy-final-root-workspace-20261004.log`, `/tmp/deppy-final-runtime-hang-20261004.txt`. 이는 최종 전체 테스트 통과 기록이 아니다.


- 최초 9개 통합 소스: workspace 4,754 passed / 0 failed / 47 existing ignored, strict workspace all-target Clippy 및 fmt 통과. 이후 리뷰 수정 전의 결과이므로 최종 산출물 검증으로 사용하지 않는다.
- PR13 최종 격리 소스: runtime 335·session 74·PTY 45 passed, PTY 1 existing ignored; 관련 strict Clippy/fmt 통과.
- PR14 최종 격리 소스: 실제 private runtime 10개 및 영향 회귀/strict Clippy/fmt 통과.
- 최종 coherent 소스 전체 테스트, 직접 입력/다중 파일 후속 결과, source commit과 0.6.0 산출물 버전 검증: 진행 중.

공개 GitHub push나 현재 앱 교체는 이 작업에 포함하지 않았다.
