# 일곱 개선 구현·전후 측정·최종 리뷰

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | alacritty_backend.rs / PR7 | 최초 snapshot 중앙값 +20.7%, 전면 변경 +3.8% | 행 Arc 생성 비용; 전면 변경 시간 구간은 겹침 | cold·밀집 출력은 실사용 GUI에서 추가 확인 |
| low | workspace.rs 테스트 | 원본에도 있던 기대값 실패 2건 유지 | Workspace 전체 검사는 실패 | 상태 문구·attached 폭 계약을 별도 검토 |
| low | AGENTS.md | Deppy GUI 실행 허락 미수신 | native IME·앱 RSS·GPU 효과 미검증 | 현재 작업의 명시적 실행 허락 후 계측 |
| low | libghostty-vt-sys / Ghostty | 실제 SDK27.0의 Zig libSystem 링크 실패 | 선택 백엔드의 native 링크·실행 미검증 | 호환 native toolchain에서 별도 검사 |

## 완료한 작업

일곱 PR의 제품 코드를 구현하고, 실제 코드의 테스트·release 프로브·CLI 코드 리뷰·재빌드를 실행했다. 서브에이전트 3개는 서로 다른 작업 트리와 `target`을 사용했다. 원본 작업 트리의 기존 변경과 실행 중인 PID71086은 유지했다. Deppy를 실행·재실행하거나 GitHub에 push하지 않았다.

- 기준 코드: `78f054b9`, detached `deppy-sijo-perf-baseline`.
- 통합 코드: `5e30a4f1`, `feat/measured-seven-improvements`.
- 통합 작업 트리: `/Users/jr/Desktop/projects/deppy-sijo-performance`.
- 빌드: `target/release/deppy-sijo`, 최종 release build exit0, 24.09초.
- 상세 데이터: [측정 summary](measurements/2026-09-27-seven-improvements/summary.json), [실행/바이너리 manifest](measurements/2026-09-27-seven-improvements/manifest.json), [검증 명령과 결과](measurements/2026-09-27-seven-improvements/tests/summary.json).

## 7개 각각의 이전·이후

시간은 300×80 실제 backend/egui release API를 같은 fixture로 **전후 교대로 3회 실행한 중앙값**이다. 메모리는 System allocator가 요청한 누적 바이트를 센 것이다. PTY feed 비용은 snapshot 시간에서 제외했다. 앱 전체 CPU/RSS, native macOS IME, GPU 실행 시간은 측정하지 않았다.

| PR | 변경 | 이전 | 이후 | 실제 검증 |
| --- | --- | --- | --- | --- |
| 1 | 압축 히스토리 heap 집계 O(1) | 20k줄 조회 28.205µs; 길이에 비례 | 0.0026µs; 1k/5k/20k 거의 일정 | mutation/clone/rotate/resize oracle, vendor155 및 예산 검사 |
| 2 | 압축 행 scratch 재사용·경량 메타데이터 | 압축 snapshot83회/1,351,216B; replay·prompt 이동 snapshot 각1회 | PR2 단독83→3회/775,216B; 메타데이터 경로 각0회 | 실제 RED→GREEN, 잔여 extras 초기화·포인터 재사용·session70 |
| 3 | 희소 결합 문자 보존 | `가ᇹ`→`ᄀ`, `a + U+0301 + U+0308`→`a` | 표시·복사·검색·원격 delta·웹 canvas·MCP 화면 읽기에 전체 문자 보존 | 실제 backend/원격/Chromium/MCP RED→GREEN; malformed 데이터 거부 |
| 4 | 같은 스타일 한글/CJK run 병합 | 한글11,852 shapes, draw+tess0.52ms | 81 shapes, 0.07ms; CPU 중앙값86.5% 감소 | 같은 D2Coding fixture, wide/emoji/fallback/스타일/선택 경계 검사 |
| 5 | IME range·같은 pass 표시·후보 위치·소유권 | range 버림, text shape2개, 같은 pass 조합 누락 | char range 보존, text shape1개, 같은 pass 표시, 잘못된 포커스 회수 차단 | renderer 및 실제 headless TextEdit RED→GREEN; 한글15/IME67/preedit8 |
| 6 | 셀12B·기존 직렬화 | Cell16B; 300×80 payload384,000B | Cell12B; payload288,000B,25% 감소 | 128 flag 조합, 원래6필드 postcard/JSON 바이트 일치 |
| 7 | 변경 행 공유 | dirty1 snapshot84.766µs/775,216B; 커서 이동85.216µs/775,216B | dirty1 2.655µs/6,256B; 커서 이동2.515µs/0B | 변경1행 생성·이전 snapshot 불변·누적dirty·resync·희소문자·flat wire·숨김 반납 |

PR1의 약2.6ns 조회는 이 마이크로벤치의 측정 바닥에 가깝다. O(1) 변경과 무할당을 확인한 결과로 해석한다. PR4의 shapes는 painter shape 수이며, GPU drawcall 감소율을 산출하지 않았다.

PR2 단독 결과와 PR6/7이 함께 적용된 결과를 나누어 기록했다. PR7 이후에는 전체 행이 달라질 때 행별 Arc를 생성하므로, 압축 snapshot의 할당 횟수만을 최종 전체 개선율로 사용하지 않는다.

## 통합 snapshot 측정 전체 결과

| 시나리오 | 이전 시간 µs | 이후 시간 µs | 이전 요청 B | 이후 요청 B | 이전/이후 할당 회수 |
| --- | ---: | ---: | ---: | ---: | --- |
| 첫 snapshot, cold | 96.833 | 116.833 | 775,280 | 299,280 | 4 /87 |
| 실제 커서만 이동 | 85.216 | 2.515 | 775,216 | 0 | 3 /0 |
| 실제 한 행 수정 | 84.766 | 2.655 | 775,216 | 6,256 | 3 /4 |
| 실제80행 수정 | 85.080 | 88.287 | 775,216 | 291,920 | 3 /83 |
| 같은 압축 viewport 다시 읽기 | 135.964 | 0.052 | 1,351,216 | 0 | 83 /0 |
| 압축 viewport 스크롤 | 130.239 | 131.930 | 1,351,216 | 299,200 | 83 /85 |

- cold는 각 실행에서 첫 호출1회를 측정한 값으로, 앱 시작 시간의 증가율을 뜻하지 않는다.
- 전면 변경의3회 범위는 이전84.38~89.25µs, 이후88.04~92.98µs다. 압축 스크롤도 이전128.74~133.56µs, 이후129.93~135.08µs로 겹친다. 작은 시간 차이를 확정적인 실사용 회귀/개선으로 판정하지 않는다.
- PR7에는 backend가 보유하는 **297,920B의 cache heap**이 새로 생겼다. 동일 fixture의 backend 예산 추정치는2,972,336→3,270,256B다. 이것을 `cache_footprint`에 O(1)로 포함하며, Hidden/Exited 전환 및 그 상태에서의 명시적 읽기 이후 backend cache를 반납한다. UI가 이미 보유한 이전 snapshot은 계속 유효하다.
- dirty1의 요청 바이트 감소는99.2%, cursor-only의 cell payload 할당은0이다. 보유 메모리와 요청 할당량을 혼동하지 않는다.
- 여러 행을 한 contiguous slice로 요청하면 lazy flat cache가 생길 수 있으며 그 heap도 예산에 포함한다. 행 단위 렌더/chunks와 wire sequence 직렬화는 평탄화하지 않는다. 현재 remote delta 재구성은 수신 측의 flat 데이터 복사를 유지한다.
- ASCII draw+tess 중앙값은0.18→0.15ms였지만 범위가 이전0.16~0.19, 이후0.14~0.17ms로 겹쳐 별도 속도 개선율을 확정하지 않았다.

## 테스트 이전·이후

| 검사 | 원본78f | 최종 통합 |
| --- | --- | --- |
| terminal 전체 | 95통과·1실패·4ignored | 119통과·0실패·4ignored |
| session 전체 | 68통과 | 70통과 |
| runtime 전체, mock keyring/4threads | 310통과 | 317통과 |
| Workspace 전체 | 264통과·2실패 | 275통과·같은2실패 |
| App 한글 | 13통과 | 15통과 |
| web-remote 전체 | 기준 측정은 개별 RED/계약 검사 | 318통과·1ignored |
| 실제 Chromium viewer/canvas | grapheme 분리 RED | 명시적`--ignored` 실행2통과 |
| App IME/preedit/cloud | 각 새 회귀 RED 별도 보존 | 67 /8 /13통과 |
| vendor 전체 | mutation·decode 회귀 RED 별도 보존 | 155통과 |
| boundary/dependencies/fmt/whitespace | — | 모두 exit0 |
| release rebuild | 원본 실행 파일 보존 | exit0,24.09초 |

Suite 간 겹치는 테스트를 합산하지 않았다. `web-canvas` 최초 기본 실행은 ignored2개였으며 그 로그를 남긴 뒤 `--ignored`로 실제2개를 실행했다.

원본 terminal 실패는 합성 available rect10000px에 그리드의 우측3px 여백을 기대한 잘못된 fixture였다. 실제 clip/padding 계약으로 fixture를 수정했다. 제품 여백 결함을 수정했다고 주장하지 않는다.

Workspace의 기존 실패는 다음 둘이다. 원본 detached 소스와 최종 통합에서 동일 결과를 확인했다.

1. `agent_info_line은_전송배지_없이_provider와_모델을_보여준다`: 기대`Idle`, 실제`Awaiting instruction`.
2. `attached_without_snapshot은_workspace_specific_unavailable을_표시한다`: 기대 폭80, 실제116.

두 계약의 별도 판단이 필요해 통과시키기 위한 기대값 변경은 하지 않았다. [원본 로그](measurements/2026-09-27-seven-improvements/baseline-tests/deppy-seven-exact-before-workspace-full.log), [최종 로그](measurements/2026-09-27-seven-improvements/tests/app-workspace.log).

## 코드 리뷰 및 수정

실제 제품 source diff만 Codex CLI로 리뷰했다. 계획·보고서·일지는 리뷰 입력에서 제외했다.

| 발견 | 수정 | 실제 확인 |
| --- | --- | --- |
| Ghostty metadata 질의 실패 시 기존 fallback 차이 | 기존 false fallback 유지, 현재 typed Screen API 사용 | source review 및 native-skip Rust check |
| 원격 sparse text`ab`가 한 셀로 승인 | native 폭0 suffix 보존, 나머지는 owner 폭 안의 단일 문자군 검증 | keyframe/delta RED2→GREEN2; 합법 native7case 유지; 재리뷰 지적 없음 |
| range 없는 IME의 caret0·잘못된 후보 anchor | visual caret은 조합 끝, 후보 anchor는 기존 terminal cursor | renderer RED2→GREEN, 재리뷰 후 후속 소유권 결함 별도 수정 |
| TextEdit 소유 IME의 같은 frame 터미널 표시 | preview·claim·PTY admission을 소유권으로 제한 | 실제 headless frame RED→GREEN |
| 프레임 후반 자동 refocus가 TextEdit 조합을 끊음 | 같은 guard로 자동 focus 소비 보류, 명시적 click 유지 | 실제 TextEdit focus/후보/PTY/한 번의 pending 소비 RED2→GREEN2; 최종 재리뷰 지적 없음 |

PR7 source 리뷰는 row immutability, dirty 합산, palette/scroll/alt resync, sparse 갱신/삭제, direct Arc 초기화 safety, flat wire 직렬화, O(1) heap 계상, Hidden/Exited 반납, exclusive end를 확인했고 추가 지적이 없었다. 상단에 남긴 검증 제한과 기존 실패를 제외한 **현재 개선 코드의 미수정 리뷰 결함은0건**이다.

추가 검토 중 sidebar 결합 문자 누락, 짧아진 pane의 후보 rect, Unicode cursor 비교의 임시 String 할당, 행 Vec→Arc의 이중 할당,1열 화면 마지막 dirty cell 누락을 실제 테스트/계측으로 수정했다. direct Arc 초기화는 모든 cell slot을 쓴 뒤 `assume_init`하는 작은 경로로 제한하고 safety 주석과 full oracle를 검증했다.

## PR별 주요 통합 커밋

| PR | 주요 커밋 |
| --- | --- |
| 1 | `4dc3cbf0` |
| 2 | `2d0d1c27`, `c5858826`, `5edf7617` |
| 3 | `d30b87d9`, `0150efd2`, `2fc88d07`, `1291fefc`, `396f15a2`, `5ee9218d` |
| 4 | `389fef36`, `890b08cb` |
| 5 | `24bc1859`, `1ba4931c`, `0c6ae730`, `da4f35a5`, `d55ee0aa`, `5e30a4f1` |
| 6 | `e440d452`, `5af4571a`, `e4053eec` |
| 7 | `ebddc67f` |

## 실패했던 검증 접근과 경계

- 초기 공용`CARGO_TARGET_DIR`가 타 작업 트리의 test executable을 덮어썼다. 해당 suite 결과를 무효화하고 작업 트리별 APFS clone target과 targeted clean으로 분리한 뒤 원본/수정 테스트를 재실행했다. 최종 결과는 분리된 실제 소스 기준이다.
- 기본 runtime 테스트의 keyring2개가 native keychain 경로에서 멈췄다. 자체 test PID만 종료하고 `secret/test-keyring-core`와4threads로 전체 검사했다. 한 번 발생한 FD 상한 실패도 보존하며, fresh baseline과 최종317개는 실제 통과했다.
- CLI의`gpt-5.6`는 현재 계정에서 지원되지 않아 현재 설정의`gpt-6-sol`을 사용했다.
- 선택 Ghostty는 Zig 부재 뒤 임시 공식0.15.2 배포본의 SHA를 확인해 실제 SDK27.0으로 빌드했지만 libSystem 링크가 실패했다. SDK 위조나 전역 설치는 하지 않았다. upstream`DOCS_RS=1` native-skip의 Rust library/test 코드 타입 검사만 통과했고 기존 API 오류 및 새 metadata 오류를 수정했다. skip-native cache를 정리했다. FFI 링크/실행의 성공 증거는 없다.
- AGENTS.md는 현재 작업의 명시적 허락 없이 Deppy 실행을 금지한다. 격리 release GUI 실행 허락을 요청했으나 답변이 없어 GUI/native 두벌식/앱 CPU·RSS/GPU 검증은 실행하지 않았다. 준비된 GUI runner와 측정 한계는 기존 계획에 보존했다.

재현용 headless 명령과 raw 로그는 `measurements/2026-09-27-seven-improvements/`에 보존했다. 원본/수정 바이너리는 `/tmp/deppy-seven-comparison-20260927/`에 보관하고 manifest에 SHA256을 기록했다.
