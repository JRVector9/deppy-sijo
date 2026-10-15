# 전체 리뷰 결함·개선 후보 수정 검증 — 2026-10-14

발견한 R01–R12, P01과 조건부 C01/C02의 필요한 수정을 소스에 반영했다. 통합 검사에서 찾은 두 원인도 보완했고, 전체 검사와 영향을 받는 범위의 재검사를 합쳐 최신 target 결과는 **5702 PASS / 0 FAIL / 65 ignored**다. 정적 검사 결과는 아래에 기록한다.

기준은 `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test `0.12.1`이다. 기존 미커밋 수정은 시작 snapshot 기준으로 보존했다. 오래된 nominal `deppy-sijo` checkout, 실제 사용자 앱·세션·DB·키·clipboard는 변경하지 않았다. 이번 요청은 코드 수정이며 commit/push/release/restart를 수행하지 않았다.

GPT-6.1-Sol/xhigh 서브에이전트 3명을 별도 worktree에서 병렬 실행했다. lifecycle, documents, data 담당의 파일 소유권을 분리했고 Root가 공통 App 연결, 단일 Cargo 실행, 기준 snapshot과의 통합 대조 및 교차 리뷰를 맡았다. 원래 발견과 재현은 [전체 코드 리뷰](../reviews/2026-10-14-full-code-review.md), 작업 순서는 [계획](../superpowers/plans/2026-10-14-full-review-fixes.md)에 있다.

## 항목별 수정

| 항목 | 반영 내용 | 검증 경계 |
| --- | --- | --- |
| R01 | Codex stdin 비차단 쓰기·5초 전체 시한·취소, child 종료/reap 후 worker/readers join | 실제 private child의 막힌 stdin과 정상 큰 RPC |
| R02 | 문서는 nonblocking open 후 같은 handle의 regular-file/byte 검사; Claude usage는 bounded worker/cache로 이동 | FIFO, 정상·과대 파일, UI frame 100회 비대기, 캐시 만료·실패 후 fallback |
| R03 | temp 작성·sync 완료 후 원래 alias/target revision을 다시 검증하고 conflict 반환 | temp 작성 중 외부 변경, source와 외부 내용 보존 |
| R04 | dirty 문서 닫기 보호, dead owner 문서의 명시적 복구, 최종 전달까지 원래 runtime/pane/session fence 유지 | 취소·저장 실패·worker panic·runtime 교체·bounded queue 재시도 |
| R05 | IMMEDIATE migration lock 안에서 SQLite online backup으로 WAL 포함 snapshot을 완성한 후 게시 | live reader/WAL row, 이전 backup 링크, 게시 실패 시 migration 차단 |
| R06 | 고유 temp 배타 생성, 동일 handle write/sync, 기존 권한과 자기 temp cleanup 보존 | 이전 고정 temp 링크 및 4개 worker의 120회 동시 저장 |
| R07 | 동일 persistent UUID의 queued write를 flush한 후 direct upsert | 이전 queued exit 후 respawn DB 상태, 기존 archive/launch 연결 |
| R08 | 링크 문서는 읽었던 canonical target에 저장하고 alias와 target을 함께 재검증 | 링크 보존·원 target 저장·외부 교체 conflict |
| R09 | shell과 실제 foreground 그룹의 birth/PGID/SID를 검증해 STOP/CONT; 실패 rollback과 frozen kill/Drop 보완 | 실제 foreground 정지, fork 경쟁, rollback 실패, fixture 작업 종료 |
| R10 | reader marker·기존 exit sentinel·응답 queue tag로 종료 전/후 query 소유권 분리 | reader backpressure, 지연 pump, marker split, 정상 새 query/사용자 입력, stamp 실패 |
| R11 | 실제 TextEdit 삽입 전에 UTF-8 개별 8MiB·전체 retained 24MiB budget 적용 | paste 선택 보존, IME, replacement, undo, aggregate budget |
| R12 | enqueue 성공 뒤 import owner 소비; 혼잡/일시 unavailable 완료 payload를 App의 1-slot lane에 보존 | Full → retry → 정확히 한 번 전달, canceled/stale payload 폐기 |
| C01 | 정상 directory 검증과 Unix anchor 지원을 분리 | host portable helper 통과; Windows 실행은 별도 범위 |
| C02 | TLS/WS 아래 TCP I/O에 전체 deadline과 50ms stop 관측 slice 적용 | Plain baseline RED 및 실제 loopback Rustls 수정 후 검증 |
| P01 | process snapshot당 children adjacency를 한 번 만들어 세션 탐지에 재사용 | 동일 합성 rows/caps/order; 앱 전체 GUI 지연 측정은 아님 |

## 실측과 조건부 판단

P01은 동일 production 함수의 `rustc -O` 합성 harness로 각 조건 10회 측정했다. 새 구조에서도 찾은 descendant 수는 동일했다.

| sessions / rows | 변경 전 평균 | 변경 후 평균 |
| --- | --- | --- |
| 32 / 2,048 | 1.313ms | 0.163ms |
| 128 / 8,192 | 18.015ms | 0.622ms |
| 256 / 16,384 | 70.400ms | 1.249ms |

C01은 non-Unix probe가 `None`이고 기존 호출자가 identity를 필수로 요구하는 정적 조건을 확인했다. 실제 macOS에서 portable directory helper는 정상 폴더를 `Ok(None)`으로 받아들이고 파일/없는 경로를 거부했다. 이 결과를 Windows App 실행 결과로 일반화하지 않는다.

C02는 Plain production decoder에서 40ms receive가 약 252ms, 실제 worker shutdown이 3.108s 걸리는 baseline 실패가 있어 수정했다. 실제 loopback Rustls 검사는 새 구현의 TLS record/WS fragment/정상 송신/stop 동작을 확인한다. TLS baseline RED를 실행했다고 주장하지 않는다. 수정 후 실제 5회귀는 모두 통과했다. Plain receive 41ms(예산 40ms), TLS partial record 40ms(예산 40ms), TLS partial-record stop 20ms, worker shutdown 0ms(정수 ms 로그)였다. 연결 중 shutdown의 범위는 기존 전체 connect 시한 10초이며 50ms 취소를 보장한 대상으로 세지 않는다. production endpoint/route/credential의 BLOCKED 상태와 WebPKI trust 정책은 유지했다.

통합 검사에서 기존 macOS 메모리 테스트의 `phys_footprint < RSS` 단정도 실패했다. 실제 20회 × 5개 private child 측정에서 `<` 19회, `=` 0회, `>` 1회였다. 같은 native 호출의 footprint는 resident보다 20회 모두 컸다. Apple XNU는 두 값을 다른 ledger로 계산한다([정의와 조회](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/bsd_kern.c#L1274-L1276), [footprint 구성](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/task.c#L1226-L1242)). 제품 수치를 RSS에 맞춰 보정하지 않고, 잘못된 test invariant를 native 합계·같은 sample 내 cache·실패 시 fallback 계약 검사로 바꿨다. 공개 source commit이 현재 호스트 커널과 같은 revision이라고 주장하지 않는다.

## 실제 실행 결과

- Root usage FIFO: baseline 1 PASS / 1 FAIL → 최초 수정 3 PASS. 캐시 age 및 slow missing-result 추가 실패를 재현하고 보완한 뒤 최종 5 PASS.
- Documents 최초 5회귀: 0 PASS / 5 FAIL. 후속 worker infrastructure caller는 18 PASS / 1 FAIL, handoff fence는 20 PASS / 1 FAIL. 통합 App 신규 선택 검사: **32 PASS / 0 FAIL**.
- Data 3개 crate 전체: **108 PASS / 0 FAIL / 2 ignored**. runtime 중간 전체는 **399 PASS / 1 FAIL**(위 기존 memory invariant), web-remote 중간 전체는 **408 PASS / 0 FAIL / 13 ignored**. 최종 전체 검사는 아래에 별도 기록한다.
- Lifecycle 초기 stalled stdin/foreground/late query가 각각 실패했다. 새 post-write 검증과 status 소비자의 파일 삭제가 경쟁하는 수정 회귀도 0 PASS / 1 FAIL로 재현하고 postcheck만 제거했다. watch 등록 시 negative probe 뒤 새 stamp를 지우는 경쟁은 삭제 없는 등록과 spawn-age 검증으로 피했다. 교차 리뷰의 frozen kill/Drop 2회귀, failed STOP rollback 1회귀, 실제 reader-parked late CPR 1회귀, stamp-write failure 1회귀도 실패한 뒤 보완했다.
- native memory diagnostic: **1 PASS**, 20회 × 5개 private child. 단순 비교가 제품 invariant가 아님을 입증한 측정이며 새로운 제품 결함으로 세지 않는다.

전체 workspace 1차 실행은 exit 101이다. App **3213 PASS / 1 FAIL / 39 ignored**, storage **460 PASS / 19 FAIL**, storage-core **4 PASS / 2 FAIL**였다. 나머지 target은 모두 통과했다. 단위/integration/doc harness별 마지막 결과만 합산해 subprocess 결과 3개를 제외한 1차 합계는 **5679 PASS / 22 FAIL / 65 ignored**다. App 실패는 source-string 테스트의 과대 검사 범위였고, storage 21개 실패는 Root가 추가한 SQLite NOFOLLOW가 정상 `/var` 부모 symlink까지 거부한 같은 원인이었다. 명시적 symlink-parent 회귀도 **0 PASS / 1 FAIL**로 확인하고 backup 부모만 canonicalize하는 보정을 적용했다. 이전 backup의 최종 entry를 따라가지 않고 temp NOFOLLOW는 유지한다. 세 affected package 전체 재검사는 **exit 0, 3747 PASS / 0 FAIL / 42 ignored**였다. App unit **3214 PASS / 39 ignored**, storage unit **479 PASS**, storage-core unit **7 PASS**이고 관련 integration/doc도 통과했다. 전체 범위에서 각 harness의 최신 결과를 합치면 **5702 PASS / 0 FAIL / 65 ignored**다. 전체 단일 명령이 exit 0이라고 표현하지 않으며, 실패한 target만 수정 후 전체 재실행했다. 집계 코드는 lib/bin을 별도 harness로 구분하고 nested subprocess 요약을 중복 계산하지 않는다.

최종 정적 검사는 모두 실제 실행했다.

| 검사 | 결과 |
| --- | --- |
| workspace / all-targets Clippy `-D warnings` | exit 0 |
| `cargo fmt --all -- --check` | exit 0 |
| xtask `check-boundary` | exit 0, UI capability / composition-root 경계 통과 |
| xtask `check-deps` | exit 0, 27개 crate의 금지 edge / 순환 없음 |
| popup HTML inline JavaScript syntax | exit 0, 1개 script |
| Windows MSVC offline cross-check | exit 101, 미캐시 `curve25519-dalek-derive v0.1.1` 때문에 컴파일 전 중단 |

Windows 결과는 제품 컴파일 실패의 근거나 통과로 세지 않는다. C01의 host portable helper만 실행 검증했고 Windows 교차 컴파일 및 실행은 확인하지 못했다. 정적 검사 후 동등한 스타일 정리와 test-only lint allowance에 영향받는 회귀도 재실행했다: App 신규 32 PASS, SessionAux 4 PASS, 이력 경계 1 PASS, coordinator 전체 83 PASS / 2 ignored, 각 명령 exit 0. 이 재검사 결과는 전체 5702 합계에 중복으로 더하지 않는다. 상세 명령과 로그는 [증거 안내](evidence/2026-10-14-full-review-fixes/README.md)에 보관한다.

## 실패 접근과 한계

- 기존 이력 테스트는 새 문서 runtime-close helper를 이력 handler로 잘못 포함했다. 실제 UI intent와 검색 reset 5개를 유지하는 경계로 1줄만 교정했다. 테스트 전용 source_editor wrapper의 cfg도 정리했다.
- Root의 SQLite NOFOLLOW 보강은 macOS 정상 parent symlink를 거부하는 회귀를 만들었다. 실제 기존 21개 실패와 새 명시적 회귀로 확인하고 parent-only canonicalize로 교정했다.
- shared Cargo target을 다른 worktree에서 재사용할 때 App entry만 touch하면 stale PTY artifact가 재사용돼 컴파일이 실패했다. 전환 시 모든 crate entry를 touch해 소스 정합성을 맞췄다. 컴파일 실패는 행동 RED가 아니다.
- 처음 Undo 회귀는 seeded cursor가 실제 선택과 달랐다. 테스트 qualification을 고쳤고 기존 bounded editor의 동작을 통과로 확인했다.
- 최초 자연 backpressure/slow usage fixture의 PASS는 각각 reader가 marker까지 진행했거나 Publishing admission이 재요청을 가린 결과였다. 생산 경로 gate/발행 완료 조건으로 qualification한 뒤 실제 실패를 확인했다.
- offline metadata와 첫 memory probe는 미캐시 target 의존성/lock 불일치로 실행 전에 실패했다. host 의존성 lock을 정합화한 뒤 실제 테스트를 실행했다.
- strict Clippy 초기 두 실행은 소유권을 유지하는 큰 enum / 거부 completion Result와 동등한 스타일 lint로 실패했다. 한 슬롯 inline command 및 정확한 sensitive payload 소유권에는 이유가 있는 좁은 item allowance를 적용하고 let-chain / filter+map 표현을 정리했다. 전역 lint 완화나 payload 복제 없이 최종 workspace all-targets 검사가 통과했다.
- R03의 마지막 revision 검사와 rename 사이에는 OS filesystem compare-and-swap이 없다. 비협조적인 외부 writer가 그 짧은 간격에 변경하는 모든 경쟁을 원자적으로 막는다고 주장하지 않는다.
- 실제 사용자 GUI/App → PWA, provider 계정, OS sleep 및 Windows/Linux 전체 실행은 이번 fixture 검증 범위 밖이다. 앱을 재시작하지 않았고 사용자용 release artifact도 만들지 않았다.

## 통합·보존 확인

최종 source snapshot은 422개 SHA를 기록한다. 시작 412개 파일 중 기존 25개가 변경됐고 누락은 0개다. locale 5개에는 입력 한도 안내 key 하나씩만 추가했고, 신규 module 3개와 synthetic TLS DER fixture 2개를 별도 기록했다. 기존 dirty 제품 파일 6개는 시작 snapshot과 byte-identical이다. 공통 App/runtime/workspace 변경은 worker의 전체 HEAD diff 대신 시작 snapshot의 증분 patch로 통합하고 Root가 최종 호출부를 검토했다. lifecycle↔data, documents→Root, data→최종 document handoff 및 Root storage 보정 교차 리뷰를 수행했다. 최종 task text patch의 `git apply --reverse --check`도 exit 0이다.

증거 생성 도중 locale를 파일 끝 추가로 가정한 검사가 실패했지만 실제 구현은 관련 key 옆의 한 줄 삽입이었다. 새 key 한 줄만 제거하면 5개 locale 모두 HEAD bytes와 같음을 확인하도록 증거 검사를 교정했다. 제품 코드 변경이나 테스트 실패로 세지 않는다. native 근거 manifest와 task manifest의 같은 basename 충돌도 최종 SHA 검사에서 발견해 고유 이름으로 보관하고 원본 SHA를 재확인했다. 최종 검사는 실제 exit 0이다: manifest 74개 저장 SHA와 gzip 원본 SHA, 현재 source 422개 SHA, 기존 dirty 6개 보존, 문서/로그 72개 whitespace·newline, 상대 파일 링크 30개, tracked diff와 증분 patch reverse check를 확인했다. 증거 manifest 자체의 자기 hash는 포함하지 않는다. 실행 로그는 증거 README가 가리킨다.
