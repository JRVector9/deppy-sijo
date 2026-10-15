# 병목·클라우드 재리뷰 결함 병렬 수정

2026-10-15. [원본 리뷰](../reviews/2026-10-15-resource-cloud-code-review.md)의 R01–R06과 P01을 현재 dirty 소스 기준으로 수정했다. 소스 통합·교차 리뷰와 실제 검증을 완료했다. 아래에는 실행한 결과만 기록한다. 앱 재실행, 사용자 DB·키 변경, 실제 provider 호출, 릴리스, 커밋·푸시는 이 작업에 포함하지 않았다.

## 수정 범위와 설계

| 항목 | 원인 | 수정 | 보호 계약 |
| --- | --- | --- | --- |
| R01 종료 교착 | 실패 completion 후 stale 작업이 다시 접수됨 | shutdown 첫 단계에서 신규 관리 작업 봉인 | 이미 접수한 작업과 유한한 durable Save 후속 단계는 배출 |
| R02 전경 프로세스 잔존 | shell PGID와 job-control foreground PGID가 다름 | master close·자연 shell reap 전 실행 신원 보존, HUP→TERM→KILL | PID/birth/PGID/SID 재확인, 자기·외부 그룹 제외 |
| R03 메타데이터 노출 | list_sessions 표시 필드에 마스킹 미적용 | title/workspace_name을 현재 corpus로 마스킹한 뒤 UTF-8 크기 제한 | UUID/generation/workspace와 actor별 권한 유지 |
| R04 인증 시한 초과 | 진행 중 frame의 개별 read마다 idle timeout만 적용 | 하위 Read/Write/flush에 남은 전체 시한·stop 적용 | 정상 fragment/Pong 재개, 성공 때 기존 socket timeout 복원 |
| R05 이력 복구 누락 | bootstrap 접수를 성공으로 간주해 실패 후 재시도 없음 | initialized/in-flight 분리, 실패 후 하나의 250ms 재시도 시각 유지 | 종료 후 재시도 없음, pending·byte 한도 유지 |
| R06 종료 세션 읽기 승인 해제 불가 | live=false이면 읽기 checkbox도 비활성 | 종료 대상의 기존 읽기 승인은 해제 가능 | 재승인·입력 비활성, 다른 actor/session 권한 유지 |
| P01 반복 마스킹 병목 | 매 UI 호출마다 전체 답변 복제·재검색 | 원본 Arc identity + 단조 corpus revision으로 safe Arc 1개 재사용 | 등록·교체·lease 해제·grace 만료·fail-closed 즉시 무효화 |

GPT-6.1-Sol/xhigh 서브에이전트 3명이 관리, PTY, WebSocket·권한을 각각 별도 worktree에서 맡았다. Root가 메타데이터·캐시를 구현하고 공유 파일 통합, Cargo 실행, 범위 검사와 최종 검증을 담당했다. 관리→transport, PTY→관리, transport→Root 메타데이터·캐시 교차 검토를 수행했다. 교차 검토는 source review이며 behavioral PASS와 구분한다.

기준은 `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test version `0.12.1`과 기존 dirty 수정이다. nominal checkout은 건드리지 않았다. 시작 422개 source 중 기존 10개를 수정하고 PTY fixture 1개를 추가했다. 나머지 412개 source의 bytes를 보존했다. 이번 작업의 증분 패치는 clean HEAD 전체 diff와 구분해 보관한다.

## 회귀와 실제 검사

- 기존 리뷰에서 실제 실패한 shutdown/DB bootstrap/메타데이터/WS deadline/egui consent 회귀를 production test로 이식했다. 이전 RED 로그는 원본 리뷰 evidence에 유지한다.
- 새 safe Arc 재사용 회귀: 구현 전 compile 성공, 0 PASS / 1 FAIL, exit101. 값이 같아도 safe Arc가 매번 달라지는 것을 확인했다.
- 새 실제 PTY 회귀: 구현 전 compile 성공, normal/frozen control 2 PASS / 비동결 보호 4 FAIL, exit101. private bash/Python fixture에 15초 수명과 exact identity 구조 정리를 적용했다.
- 초기 통합 후 secret+pty 전체: exit0, secret 80 PASS, pty 64 PASS / 1 ignored. HUP와 HUP·TERM 무시 작업의 Drop/kill-alone, 신원 변경 시 신호 금지, 기존 frozen/foreign 보호를 포함한다.
- 보완 후 App cloud 전체: exit0, 175 PASS / 0 FAIL / 3 ignored. 메타데이터·캐시·bootstrap 복구·잠금 중 종료·durable continuation·종료 대상 consent UI 회귀를 포함한다.
- Web 전체: exit0, unit 392 PASS / 1 ignored + integration 20 PASS / 12 ignored, 합계 412 PASS / 13 ignored. 정상 auth/control/slot·watch 정리도 포함한다.
- 최종 보완 후 PTY 전체: exit0, 68 PASS / 0 FAIL / 1 ignored. 실제 reap 후와 reaper 관측 전의 Drop/kill, 기존 전경 정리·동결·재개·외부 그룹 보호를 포함한다. 변경하지 않은 runtime 승인·종료 handshake도 1 PASS / exit0.
- Root 독립 PTY production 재현: 최신 canonical rlib를 새 binary로 재링크해 compile0/run0. normal/HUP무시 Drop/kill/frozen 4조건 모두 고정한 전경 실행 잔존=false, 각 identity cleanup 완료. 기존 review executable을 재사용하지 않았다.
- 최종 workspace 단일 명령: exit0, **5732 PASS / 0 FAIL / 65 ignored**. Cargo harness 78개별 마지막 결과를 사용하고 nested subprocess 3건은 중복 제외했다. 앞선 affected 결과를 더하지 않았다.
- 엄격한 `cargo clippy --workspace --all-targets -- -D warnings`, 전체 fmt check, xtask check-boundary/check-deps 모두 exit0. 정확한 offline/locked 명령과 로그는 evidence에 보관했다.

## 성능 실측

원본과 수정본의 **실제 production method/cache type 및 redaction source**를 같은 합성 fixture로 `rustc -O` 컴포넌트 측정했다. GUI frame, 전체 앱 release, RSS 또는 retained heap 측정이 아니다. setup/output drop은 timed region 밖이며, timed call의 할당과 Arc publication은 포함한다.

100개 답변 × 각 16KiB / raw key 32개 / 같은 source·corpus, 10 sample:

| 지표 | 수정 전 매 호출 | 수정 후 캐시 hit |
| --- | ---: | ---: |
| 평균 처리 시간 | 110.782ms | <0.001ms |
| 호출당 할당 | 14,902건 | 0건 |
| 호출당 누적 할당 bytes | 37,102,106 | 0 |

늦게 등록한 키·줄바꿈으로 감싼 키 마스킹, routing identity 보존과 warm Arc identity contract도 실제 PASS다. 로그 정밀도는 소수점 3자리이므로 0ms라고 단정하지 않는다.

새 source/corpus의 첫 재계산은 동기식이다. 위 조건의 cold projection은 114.108ms, raw key 128개는 387.129ms였다. corpus snapshot mutex를 유지해 한 결과의 필드들이 같은 corpus를 사용하며, 그 재계산 동안 키 등록이 기다릴 수 있다. 이번 수정은 **변하지 않은 데이터의 반복 비용**을 제거한다. 모든 cold GUI 지연을 제거했다고 주장하지 않는다.

## 검증 중 교정한 접근

- 첫 App GREEN 시도는 새 UI test의 `NodeT` trait import 누락으로 compile101이었다. 실제 행동 RED가 아니다. import를 보완하고 원 로그를 보존했다.
- 다음 App 실행은 174 PASS / 1 FAIL / 3 ignored였다. 새 accepted-stale fixture가 bootstrap을 완료하지 않아 bootstrap+stale 두 작업을 접수한 뒤 completion 하나를 기대했다. 실제 로그는 223ms에 두 accepted 작업을 배출하고 종료했음을 보여준다. fixture의 empty bootstrap 완료와 초기 3회 wake callback barrier를 추가했고 exact-one assertion을 유지했다. 보완 후 App cloud 전체가 통과했다.
- 첫 workspace 실행은 runtime 자연종료 승인/descendant handshake 검사 1건이 실패해 exit101이었다. 새 PTY 안전 검사가 이미 회수된 셸의 신원을 확인할 수 없어 기존 same-group 자식 TERM 정리를 누락한 실제 회귀였다. 실패 fixture는 Root가 private 경로·command·PID/birth/PGID/SID를 재검증하고 개별 TERM으로 정리했다. 새 Drop/kill 회귀 2건도 실제 RED를 확인했다. 첫 zombie-query 보완은 macOS에서 exited-unreaped PID를 proc_pidinfo/getsid가 ESRCH로 반환해 다시 실패했다. Root native probe로 입증하고, child mutex 아래 matching waitid WNOWAIT 결과로 회수 전 멤버 신원을 보존하도록 교정했다. 첫 waitid 시도는 61 PASS / 5 FAIL / 1 ignored였으며, 같은 native probe에서 WEXITED에도 CLD_STOPPED가 반환됨을 확인했다. 비종료 이벤트를 보류하고 teardown 입구에서도 상태를 관측한다. 실제 reap 증거를 ECHILD로 강화하고 reaper 정지 상태의 즉시 Drop/kill 회귀 2건도 추가해 실제 RED 후 수정했다. 최종 PTY 전체 68 PASS / 1 ignored, 변경하지 않은 runtime 승인 handshake 및 최종 workspace가 통과했다.
- transport의 초기 read-only 시한 wrapper는 source review에서 automatic Pong/Close Write/flush 경로까지 확장했다. 성공 시 read/write timeout 복원과 실패 후 best-effort reply의 같은 시한도 검사한다.
- 선택적 gstack review workflow의 필수 checklist가 로컬에 없어 그 workflow는 미완료다. 대신 명시적으로 요청된 직접 source/patch 교차 리뷰를 수행했다. 자동 리뷰 승인이나 실제 provider 통합 검증으로 표현하지 않는다.

## 범위와 다음 작업

정상 종료는 snapshot 시점에 고정한 실행만 정리한다. 이후 생성되거나 session을 벗어난 background descendant까지 전부 정리하는 계약은 추가하지 않았다. 실제 macOS PTY와 private SQLite/loopback/egui fixture를 검증하며, 실제 cloud provider 계정·public tunnel·실휴대폰·OS sleep·Windows/Linux runtime 호환성 검증과 구분한다. 기존 Relay BLOCKED가 해소됐다고 주장하지 않는다.

이 문서의 source/test 수정은 사용자에게 배포하는 release가 아니다. 다음 사용자용 build를 배포할 때 last-shipped보다 높은 patch version과 binary/macOS bundle version 검증이 필요하다. 새 사용자 재실행 요청 없이는 앱을 stop/launch하지 않는다.

최종 실행 로그·patch·SHA·peer review는 [evidence README](evidence/2026-10-15-resource-cloud-fixes/README.md)에 모은다.
