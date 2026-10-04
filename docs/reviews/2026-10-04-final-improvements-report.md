# 성능·메모리·에이전트 터미널 개선 결과

상태: **코드 구현·독립 리뷰·최종 전체 테스트·0.6.0 재빌드 및 패키지 검증 완료.** 완료일은 2026-10-05(KST)이며 문서·로그 경로는 작업 시작일을 유지한다. 현재 실행 중인 앱은 종료하거나 재실행하지 않았다.

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

기존 private secret-env fixture의 timeout은 baseline에서도 발생했다. 중간 실패 기록을 보존하고 실패 시 자체 테스트 이벤트/행/저장소 상태를 확인하는 진단만 추가했다. 해당 단계의 전체 runtime gate는336 passed였으며, 과거 timeout 원인을 모두 확인했다고 주장하지 않는다. 이후 확인한 native 키체인 fixture 문제와 최종 Runtime342 통과 결과는 아래에 구분해 기록한다.

## 후속 수정 결과

| PR | 변경 전 | 변경 후 실제 검증 |
|---|---|---|
| 10 직접 AI 터미널 입력 | UI에서 만든 입력을 다음 logic pass에서 전송; 큐 포화 시 소유 bytes 유실 | 첫 pass0B/다음6B → 첫 pass6B; 원래 bytes/세션의 미전송 Busy만 제한 재시도; App14·Workspace340 및 runtime/경계/strict 통과 |
| 11 파일트리 선택 전체 작업 | 실제 Shift marquee3개가 드래그에서1개로 줄어듦;16개 파일 한도 | frozen3개 이동/Option복사;64개 전체 복사·이동·삭제;20named·FileTree181 및 영향 회귀/strict 통과 |

PR10은 빠른 연속 입력을 합칠 때 버퍼 확장256회→9회도 확인했다. 실제16ms 재시도가 egui에서 즉시 repaint가 되는 경우를 재현·수정했다:0µs→15,867µs. 한글 IME Commit+Enter가 같은 pass에서 정확한 한 입력으로 private cat까지 전달됐으며, 단일 fixture host356µs/echo3,223µs다. 실제 실행 중인 Grok/provider 화면을 조작하거나 측정한 값은 아니다. 채널 admission과 이후 PTY acceptance는 구분하며 이미 접수된 입력을 무조건 재전송하지 않는다.

PR11은 공통 상한4,096개/경로합계1MiB/개별32KiB, 조상 중복 제거, 전체 충돌·위험 경로 preflight, 원래 tree root/generation과 연속 삭제 재확인, destructive Cmd+Option+V tree 포커스, typed DND를 적용했다. OS clipboard/Trash는 주입한 private provider로만 검증했다. 알려진 preflight 실패는 첫 원본/내용 전송 전 거절하며, 이후 OS 오류에 대한 여러 파일 rollback을 보장하지 않는다.

세부 명령·실패 기록·측정 범위: [직접 입력 PR10](2026-10-04-pr10-direct-input-latency.md), [다중 선택 PR11](2026-10-04-pr11-tree-multiselect.md). 두 clean commit을 실제 root index 변경 없이 통합했고 아래 corrective PR도 추가했다. 초기 후속 소스 freeze `f875919b3d0fe9355f51126bba30b29cf6681eda`는 역사적 검증 기준이며 최종 제품 소스는 아래에 기록한다.

## 마지막 리뷰와 실제 테스트 오류 수정

후속 PR의 독립 CLI는 실제 backend 이벤트와 디스크 파일명 규칙에서 두 버그를 확인했다. PR11r에서 모두 수정했고, 추가 검토에서 확인한 키보드 배열과 테스트 가정도 PR20에서 수정했다.

- macOS `⌘⌥V` press가 egui-winit에서 Paste 또는 무이벤트로 바뀌는 경로를 기존 native monitor의 별도 이동 제스처로 처리한다. 식별 없는 Paste는 이동 시작을 승인하지 않으며 다른 입력 소유자에서 시작한 반복도 나중에 트리로 연결되지 않는다.
- 목적지 볼륨이 대소문자/유니코드 표기를 같은 이름으로 취급할 때 전체 선택 충돌을 원본/내용 전송 전에 거절한다. 알려진 APFS/HFS ASCII 경로는 임시 probe 0개, 유니코드/알 수 없는 볼륨은 제한된 목적지 probe 후 정리한다. 임시 probe 자체는 metadata 효과다.
- Dvorak 등의 논리 K가 물리 V 위치에 있을 때 `⌘⌥K`로 이동을 실행하던 경로는 이동 전용 classifier로 거절한다. 기존 일반 붙여넣기·복사는 유지한다.
- 회귀 테스트는 디스크를 case-insensitive라고 가정하지 않고 실제 create-new 이름 규칙을 관찰한다. 동등한 이름의 전송 전 거절과 서로 다른 ASCII/한글 파일의 전체 복사·이동 성공을 모두 검증했다.

전체 검증 중 기존 Runtime fixture가 native 키체인 안에서 대기하는 문제도 확인했다. 실제 테스트 프로세스 sample은 `SecretStoreResolver→KeyringSecretStore→SecItemCopyMatching` 경로였다. 기존 전역 mock 등록이 의존 crate의 macOS 구현을 대체하지 못했다. PR18은 테스트 fixture마다 새 메모리 저장소를 명시적으로 주입했다. 제품의 secret resolver·저장소는 변경하지 않았고, 실제 private PTY의 환경 주입·로그 redaction 검증과 timeout도 보존했다.

실패 기록은 보존했다. `/tmp/deppy-final-root-workspace-20261004.log`의 Runtime 제외4,475 통과 뒤 Runtime 대기로 종료101인 결과를 전체 통과로 사용하지 않는다. 과거 secret-env timeout의 원인을 모두 같은 문제로 확정한 것도 아니다.

마지막 독립 리뷰는 `gpt-6.1-sol` / `xhigh`, 읽기 전용으로 실제 작은 수정분과 호출자를 확인했다. **확인된 도입/미해결 지적 없음**: `/tmp/deppy-pr20-final-cli-result-20261004.txt`. 이전 전체 리뷰·후속 리뷰의 모든 확인된 지적은 각 corrective PR의 실제 RED/GREEN을 거쳐 수정했다. [PR11r 기록](2026-10-04-pr11-tree-multiselect.md), [PR18](2026-10-04-pr18-runtime-secret-test-isolation.md), [PR20](2026-10-04-pr20-move-layout-volume-fixtures.md).

## 최종 통합 검증

최종 제품 소스 스냅샷: `d1818e3355e9604998e6581285bad8e7edd88ffb`.
제품 소스 커밋: `c532ce0ad2c5edcc9e3dcbc779a61b37d5ea53a2`.
두 상태의 `crates/`, `xtask/`, `Cargo.toml`, `Cargo.lock`은 동일하다. 기존 실제 HEAD/index·미커밋 소스는 보존하고 로컬 통합 브랜치 `feat/audit-nine-pr-v0.6.0-20261004`에 커밋했다. 각 PR의 별도 커밋/브랜치도 보존했다. 공개 push는 하지 않았다.

| 검증 | 최종 실행 결과 |
|---|---|
| workspace, Runtime 제외 |4,484 passed /0 failed /47 기존 ignored |
| Runtime, 직렬 |342 passed /0 failed /0 ignored |
| 합계(서로 겹치지 않는69그룹) |**4,826 passed /0 failed /47 기존 ignored** |
| workspace all-target Clippy `-D warnings` |exit0, 새 lint allowance 없음 |
| fmt / diff check |exit0 |
| UI boundary |bounded composition-root tail 포함 통과 |
| 의존성 |27 crates, 금지 edge/순환 없음 |

실행 로그: `/tmp/deppy-final-pr20-root-workspace-20261004.log`. Cargo 공유 target의 잘못된 binary 재사용을 막기 위해 compile부터 테스트 종료까지 자체 gate 잠금을 유지하고 작업트리 전환 때 workspace artifacts를 정리했다. 검증 중 제품 소스를 변경하지 않았다.

정확한 최종 명령은 gate가 다음 Cargo 배열을 순차 실행한다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["test","--offline","--locked","-q","--workspace","--exclude","runtime","--","--test-threads=8"],
 ["test","--offline","--locked","-q","-p","runtime","--","--test-threads=1"],
 ["clippy","--offline","--locked","--workspace","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"],
 ["run","--offline","--locked","-q","-p","xtask","--","check-boundary"],
 ["run","--offline","--locked","-q","-p","xtask","--","check-deps"]
]'
```

## 로컬 재빌드

이전 배포 버전 **0.5.5 → 0.6.0**. gated offline/locked release build와 macOS package verification 모두 exit0. 실제 workspace 멤버27개와 lock entry27개, Cargo 컴파일 메타데이터/바이너리 문자열, 번들의 `CFBundleShortVersionString`·`CFBundleVersion`이0.6.0으로 일치했다. native About은 `CARGO_PKG_VERSION`을 표시하는 소스를 확인했으며 새 앱을 실행해 About UI를 열지는 않았다.

- [최종 앱](../../target/bundle-0.6.0/Deppy%20Sijo.app)
- [최종 ZIP](../../target/bundle-0.6.0/Deppy%20Sijo.zip)
- 소스 커밋: `c532ce0ad2c5edcc9e3dcbc779a61b37d5ea53a2`
- binary SHA256: `2872566b28bf8deb47efca60d8134373a40995dab288e03989d8035e2251ad02`
- ZIP SHA256: `dc6847b6839682a79373be2b2deb61e16d0dcb4927dfefb2afc61a4fd6c898cb`

앱·두 helper의 서명/architecture/plist/license 및 ZIP 안의 동일 hash를 검증했다. 이는 명시적인 **로컬 개발 산출물** 검증이며 공증된 공개 배포 완료를 뜻하지 않는다. 로그: `/tmp/deppy-release-0.6.0-final-package-20261004.log`; 세부 메타데이터: `/private/tmp/deppy-audit-nine-pr-20261004/release-0.6.0-final-verification.json`.

앞선 검토용0.6.0 번들은 실행·배포하지 않고 `target/review-build-0.6.0-pr11r-unreleased-20261004/`에 따로 보존했다. 실행 중이던 PID21631은 그대로 살아 있고, 기존0.5.3 실행 파일·ZIP의 SHA256과 실제 Git index도 원래와 같음을 읽기 전용으로 확인했다.

실행 중인 앱은 계속 그대로다. 실제 native Grok 화면의 입력 지연, App 전체 RSS/GPU/FPS 또는 Finder 실제 clipboard/Trash를 조작한 검증은 수행하지 않았다. 위 측정은 실제 소스 모듈·private PTY·격리된 파일 작업의 범위다.
