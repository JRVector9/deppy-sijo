| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| low | command_center_observe.rs:294 | F01 기존 경합 재현 주장 철회 | 실제 host guard가 완료 처리를 차단함 | 지연 입력의 추가 방어 검사로 재분류 |
| high | command_center_host.rs:1186 | 배정 확인이 단일 메모리 토큰임 | 재시작·다음 목표 확인 후 기존 대기 작업 배정 불가 | 목표별 확인 보존·검증된 복원 경로 제공 |
| medium | ui/command_center/main.rs:500 | 검증 실패 상태의 새 워커 재시도 숨김 | 원격 작업의 수정 시도를 새로 시작할 수 없음 | host capability와 UI 표시 조건 통일 |
| medium | web/shared/command-center.js:424 | 체크박스 포커스 중 snapshot 예외 | 설정 편집 중 상태 갱신 일부 중단 | 선택 범위를 지원하는 텍스트 입력에만 복원 |
| medium | web/shared/command-center.js:238 | snapshot이 drawer 포커스를 제거 | 갱신 뒤 Escape로 닫기 실패 | 활성 drawer 포커스·복귀 대상 유지 |
| medium | command_center_host.rs:2006 | 완료 프로젝트도 캐시 퇴출 조건 불충족 | 네 프로젝트 사용 뒤 다섯 번째 로드 거부 가능 | 종료 작업의 일시 상태 정리·퇴출 조건 수정 |
| medium | command_center_host.rs:1209 | 실제 승인 기능은 연결되지 않음 | 시안의 명령·폴더·기기 승인 흐름 미구현 | 지원되는 정확한 owner API 연결 |
| medium | ui/command_center/main.rs:208 | 작업 행 109px, 시안 약62px·담당 열 소실 | 작업과 확인 항목이 첫 화면 아래로 밀림 | 3열 배정 요약·행 간격 복원 |
| medium | ui/command_center/layout.rs:126 | 작업자 drawer가 72–78px 낮게 시작 | 별도 닫기 행까지 가용 높이 감소 | 헤더 바로 아래 시작·닫기 헤더 통합 |
| medium | ui/command_center/workers.rs:352 | 기기 관리가 목록 내부에서 스크롤됨 | 시안의 고정 하단 진입점 소실 | 목록 밖에 footer 영역 예약 |
| medium | ui/command_center/mod.rs:1068 | 390px에서 이력 메뉴가 숨겨짐 | 가로 스크롤 안내 없이 주요 메뉴 누락처럼 보임 | 모바일 탭·보조 버튼 폭 조정 |
| medium | ui/command_center/settings.rs:26 | 설정 폭 836px·텍스트 입력 높이20px | 시안의 최대620px·높이36px와 다름 | 독립 제한 영역·공통 입력 높이 적용 |
| medium | web/shared/command-center.js:256 | 작업자 모니터의 필터·범위·바로가기 등 누락 | 우측에서 확인·작업 전환하는 시안 흐름 불완전 | 승인한 모니터 구조와 권한별 경로 구현 |
| medium | web/shared/command-center.js:414 | 메인의 CHECK·LIVE 영역 누락 | 확인할 작업과 최근 진행을 메인에서 볼 수 없음 | 주의 항목·작업별 최근 기록 렌더 |
| medium | web/shared/command-center.js:446 | 상단 터미널 메뉴가 설정으로 대체됨 | 시안과 탐색 경로가 달라짐 | 정확한 세션의 터미널 메뉴 복원 |

**수정 단계 정정(F01):** 실제 `command_center_remote::handoff_pending`은 원격뿐 아니라 로컬 `host.inputs`도 검사한다(remote.rs148–151). 기존 probe는 host intent를 거치지 않고 reducer를 직접 호출했으므로, 실제 사용자가 완료 뒤 입력을 받는 경합을 입증하지 못했다. F01의 high 분류·재현 주장을 철회하고 지연 입력의 방어 보강으로 재분류한다. 아래 원 리뷰는 당시 기록이며, 수정 결과는 별도 교정 보고서에 기록한다. 사용자에게 정정을 알렸다.

# Command Center v2 병렬 코드·디자인 리뷰

2026-10-20. **시안과 동일하게 구현됐다고 판단할 수 없다.** 재현·소스 확인한 주요 결함과 시안 불일치는 **정정 후14건(높음1, 중간13)**이며, 별도 F01은 방어 보강 항목이다. 네이티브·PWA·작업 실행/복구를 GPT-6.1-Sol/xhigh 서브에이전트 세 명이 병렬 검토했고, root가 핵심 경로와 증거를 다시 대조했다. 위 표의 `command_center*.rs`는 `crates/app/src/`, `ui/`는 `crates/app/src/ui/` 아래다.

검토 대상은 `feat/agent-command-center-v2-20261020`의 **9c0a820a7805c4f25a1a6674f3b657fff464eb06**. 제품 소스는 최종 검사 source d2b4605c와 같다. 기준은 [승인 HTML](../mockups/multi-agent-command-center-v2-2026-10-20.html), [디자인 명세](../superpowers/specs/2026-10-20-command-center-v2-design.md), [원본 참조 manifest](../design/command-center-v2/reference.json)다. 승인 파일6개의 hash는 모두 일치했다. 검토 결과물과 handoff만 추가·변경했으며 제품 코드는 수정하지 않았다. 앱 시작/중지/재시작, 배포 빌드, 커밋, push, main merge는 하지 않았다.

## 1. 우선 고쳐야 할 동작

**F01 — 철회된 재현 주장: 방어 보강으로 재분류.** 아래 당시 분석은 실제 host의 `handoff_pending` 보호를 누락했다. 사용자가 완료 처리를 통과할 수 있다는 근거로 사용하지 않는다. 교정 결과는 [수정 보고서](2026-10-20-command-center-v2-fixes.md)에 기록한다.

당시 주장: 저장 ACK에서의 `assignment_current` 검사는 host:1454–1460에 있지만, 이후 Codex NativeIdentity 조회를 기다리는 동안 입력이 `Observations.delayed`에 남는다. 동일 attempt를 수동 결과 등록 → 검증 → 최종 확인으로 완료할 수 있다. 콜백은 observe:283–323에서 세션·runtime·토큰·시간만 확인하며 완료된 attempt를 다시 검사하지 않는다. send:343–348도 입력 map과 cloud permit만 검사한다. `app.rs:33945–34108`의 실제 전송 경로와 runtime guard는 살아 있는 CLI/editor를 확인하지만 작업 완료 상태는 모른다. 따라서 여전히 idle인 원래 CLI라면 완료 표시 후에도 지시를 받을 수 있다.

실제 reducer/콜백 소스를 사용한 별도 harness에서 Completed 뒤 broadcast 호출을 확인했다. 이어 실제 `InputAdmission`·`AgentInputGuard`와 직접 소유한 무해한 `/bin/sh`→`/bin/cat` PTY로 입력 접수·수신을 확인했다. 두 개의 분리된 증거이며 전체 앱/유료 Codex의 단일 E2E 실행을 했다는 뜻은 아니다. 수정은 **미해결 로컬 초기 입력이 있는 동안 결과 완료를 차단하거나 정확한 원 attempt의 입력 허가를 회수**하고, 지연 전송과 접수 시점에 동일성을 재검사해야 한다. 새 operation으로 자동 재전송해서 해결하면 안 된다.

**F02 — 목표별 배정 확인을 복원할 수 없음.** host:60의 `confirmed`는 하나의 `Option`이고, 새 프로젝트를 만들면 None(host:626)이며 SQLite snapshot에는 없다. 다음 계획 확인(host:1186)이 이전 것을 덮어쓰고 설정 저장(host:1122)도 제거한다. 재로드는 PlanReview 목표만 계획 요청으로 복원(host:1383–1395)하므로 이미 확인되어 Queued인 미실행 작업은 확인 토큰 없이 남는다. 재확인도 `goal is not awaiting plan confirmation`으로 거부된다. 실제 SQLite 저장/로드와 queue/dispatch 소스로 재현했으며, 두 번째 목표의 토큰으로 첫 목표의 여전히 ready인 작업을 배정할 수 없음도 확인했다. **목표별 확인·미실행 작업의 명시적 재검토/복원**이 필요하다. 실행 중인 attempt·이미 접수한 지시는 보존해야 한다.

**F03 — 검증 실패 뒤 새 워커 재시도가 사라짐.** reducer:363–383은 VerificationFailed 재배정을 허용하고 host도 capability를 계산하지만, native main:500–506의 상태 열거에서 빠졌다. 원격 작업은 host:745 때문에 기존 로컬 eligible session이 없으므로 다른 배정 버튼도 없다. 정확한 소스 분기 두 명이 독립 확인했다. 물리 원격 기기 테스트는 하지 않았다. UI가 host capability를 사용하도록 통일하고, 검증 실패 → 새 attempt 시나리오를 검사해야 한다.

**F04 — PWA 설정 체크박스의 다음 갱신이 예외 발생.** Settings에서 `setting-use_worktrees` 체크박스에 포커스한 뒤 `work_snapshot`을 받으면 JS:399,424가 체크박스의 `[null,null]` 선택 범위를 복원하려고 한다. `setSelectionRange`는 체크박스에서 InvalidStateError를 발생시킨다. 해당 frame의 `updateComposer`, status/remote watch 동기화까지 도달하지 못한다. shared fixture와 실제 production index/번들 순서·WebSocket 처리 경로 양쪽 Chrome에서 재현했다. textarea·선택 범위를 지원하는 텍스트 타입과 숫자 범위만 복원해야 한다.

**F05 — PWA drawer가 갱신 후 포커스와 Escape를 잃음.** 390px에서 프로젝트 drawer를 열면 닫기에 포커스한다. snapshot의 `replaceChildren`(JS:238,257)이 그 DOM을 제거해 BODY로 포커스가 이동한다. keydown이 element에만 연결(JS:469)되어, 즉시 Escape를 눌러도 닫히지 않는다. 실제 Chrome/CDP에서 갱신 전 Escape는 닫힘, 갱신 후는 열린 채임을 확인했다. 이후 Tab으로 drawer 안에 다시 들어갈 수 있으므로 영구 trap 실패라고 과장하지 않는다. 렌더 후 활성 drawer 및 연결된 복귀 버튼을 유지해야 한다.

**F06 — 완료 프로젝트 네 개가 슬롯을 계속 점유.** 계획 확인 후 request/confirmed가 남으며 완료 처리도 해제하지 않는다. host:2006,2008은 둘 다 None이어야 퇴출을 허용하므로 모두 저장·완료돼도 다섯 번째 프로젝트를 거부할 수 있다. 실제 `admit_project_load` 함수와 합성 terminal-cache fixture에서 거부를 확인하고 일시 필드 제거 후 통과를 확인했다. 네 개의 실제 사용자 프로젝트를 앱에서 완료한 E2E 검사는 아니다. durable 작업 상태·미해결 effect에 따라 안전하게 퇴출하고 종료된 계획의 일시 자료를 정리해야 한다.

**F07 — 승인 화면은 실제 host와 미연결.** Task DTO는 approval을 채우지 않고, Intent::Approval은 host:1209에서 항상 거부한다. 새 워커는 PTY이며 이 transport가 명령 단위 allow/deny를 지원하지 않는 것은 기존의 의도된 제약이다. 기존 Codex AppServer의 exact request_key API는 있으나 Command Center의 structured route가 배정 가능한 실행으로 연결되지 않는다. 따라서 fixture의 승인 버튼 검사는 실기능 증거가 아니다. 이는 위험한 오승인 회귀가 아니라 **승인한 디자인의 미구현 범위**다. 정확한 owner/request를 제공하는 transport부터 연결하고, 근거 없는 PTY 승인 자동화는 하지 않아야 한다.

## 2. 네이티브: 시안과 실측 비교

| 항목 | 승인 시안 | 실제 egui fixture | 영향 / 수정 기준 |
| --- | --- | --- | --- |
| F08 배정 목록 | 번호·제목/범위·담당 3열, desktop 약62.09px/행 | provider/device를 세 번째 줄에 쌓음, 109px/행; font1.25 약121px | 담당 열·작은 본문 폰트·공통 행 간격 복원 |
| F09 작업자 drawer | 앱 헤더 바로 아래, 닫기가 모니터 헤더에 있음 | 프로젝트 바 아래로72–78px 내려감, 별도 닫기 행 약44px 추가 | header.bottom 기준으로 시작, 헤더 한 행 통합 |
| F10 모니터 하단 | 목록 밖 고정 기기 수/관리 footer | ScrollArea 안에 기기 관리 배치 | 작업자 수가 늘어도 하단 진입점 유지 |
| F11 모바일 내비 | 390px에서 다섯 메뉴 노출 | 이력 전체가 가려짐; font1.25에서는 기기 일부도 가려짐 | 작은 탭 글꼴·보조 버튼 폭 조정 또는 명시적 overflow 표시 |
| F12 프로젝트 설정 | 폼 최대620px, 텍스트 입력 높이36px | desktop 폼 입력836px, 텍스트 입력 약20px | 내용 배치 전 제한된 자식 영역·공통 높이 적용 |

기준 높이를 맞춘 HTML 파생 캡처와 비교했다. 네이티브 CHECK/LIVE는 코드에 **있지만 목록의 과도한 높이로 아래로 밀린 것**이다. PWA의 실제 누락(F14)과 구분한다. 모바일 메뉴도 source에 수평 스크롤은 있어 기술적으로 불가능한 경로가 아니라 **잘림·발견성 문제**다. 설정 폼은 `set_max_width` 호출 자체가 있어도 실제 PNG의 필드가 제한되지 않는 것을 확인했다.

네이티브 17장(기존 최종 제품 소스 fixture10 + 이번 추가7), 원본 승인 PNG3장 및 HTML 파생 참조를 열었다. Settings desktop/mobile/760px font1.25, 빈 프로젝트·무프로젝트 상태를 추가 렌더했다. 빈 상태에는 별도 추가 blocker를 발견하지 못했다. 하단 스크롤의 어두운 fade나 fixture의 프로젝트명·provider/기기 수 차이를 결함으로 세지 않았다.

낮은 우선순위의 표현 차이도 있다. 작업자 메타/시각/터미널 버튼이 시안의 양끝 정렬 대신 왼쪽에 몰리고, 기기 화면은 기술적인 TLS export 폼을 목록보다 먼저 보여주며, composer에는 두 번째 입력 테두리와 시안과 다른 대상 정렬이 있다. 이 세 항목은 주요15건 집계 밖의 추가 관찰로 남겼다. 보안 설명을 제거하자는 뜻이 아니라 승인한 화면 위계로 배치하자는 제안이다.

## 3. PWA·Relay: 구조 누락

**F13 — 우측 모니터가 축약된 다른 화면임.** JS:250–260은 제목·activity·provider/device/state/time·일반 안내만 그린다. 같은 데이터에 role/scope가 있어도 표시하지 않으며, 시안의 전체/확인 필요/실행 중 필터, 정확한 결과/승인/터미널 바로가기, 실행 규칙, 기기 관리 footer, desktop 접기를 제공하지 않는다. 전체 행 클릭은 일반 Work 탭으로만 간다. 보기 전용 Relay도 필터·메타데이터를 생략할 이유는 없다. effect는 기존 capability로 제한하면서 구조를 복원해야 한다.

**F14 — 메인의 CHECK·LIVE가 없음.** JS:414–417에는 목표·plain summary·작업 table·계획 편집만 있다. 승인/결과 확인이 필요한 작업과 작업별 이력이 있어도 CHECK 요약·LIVE 최근 진행·전체 이력 진입을 렌더하지 않는다. 실제 HTML의 동일 goal/task/scope/provider/device/activity를 snapshot으로 매핑하여 빈 데이터 때문에 생긴 차이가 아님을 확인했다. 단순히 전체 작업 화면을 메인에 중복시키지 말고 승인한 세 영역의 역할을 복원해야 한다.

**F15 — 터미널 메뉴가 설정으로 바뀜.** JS:446의 다섯 항목은 메인/작업/기기/기록/설정이다. 승인 구조는 메인/작업/터미널/에이전트·기기/이력이다. 상단의 별도 `터미널로`도 현재 정확한 세션 열기가 아니라 기존 dashboard로 복귀한다. 기존 browser test의 개수5 검사로는 이 오류를 잡지 못한다. 좌측 세션/작업 상세의 터미널 경로는 남아 있으므로 접근이 전부 불가능하다는 뜻은 아니다.

PWA 원격 워커 입력의 읽기 전용 제약, Relay의 보기 전용 정책, browser에서 새 로컬 워커/기기 설정을 직접 수행하지 않는 지원 경계는 별도로 기록했다. 이들을 JS 권한 오류로 세지 않는다. 다만 기존 문서의 제약이 원래 승인 시안을 소급 변경한 증거는 아니므로, 완전한 기기 간 기능 동등성을 완료했다고 말할 수는 없다.

## 4. 비교 이미지와 원시 증거

아래 PNG는 승인 HTML의 고정 예시 데이터와 native/shared fixture의 **리뷰용 렌더**다. 현재 사용자 앱/DB의 실제 작업 캡처가 아니다.

| 화면 | 승인 기준 | 구현 증거 |
| --- | --- | --- |
| desktop 메인 | [HTML 렌더](assets/command-center-v2-review-20261020/approved-desktop.png) | [native](assets/command-center-v2-review-20261020/native-desktop.png), [shared PWA](assets/command-center-v2-review-20261020/shared-desktop.png) |
| 390px 메인 | [HTML 렌더](assets/command-center-v2-review-20261020/approved-mobile.png) | [native](assets/command-center-v2-review-20261020/native-mobile.png), [production PWA](assets/command-center-v2-review-20261020/production-mobile.png) |
| native 설정 | [원본 HTML의 프로젝트 설정](../mockups/multi-agent-command-center-v2-2026-10-20.html) | [desktop](assets/command-center-v2-review-20261020/native-settings-desktop.png), [mobile](assets/command-center-v2-review-20261020/native-settings-mobile.png) |
| native 작업자 drawer | [원본 HTML의 작업자](../mockups/multi-agent-command-center-v2-2026-10-20.html) | [mobile font1.25](assets/command-center-v2-review-20261020/native-workers-mobile.png) |

[네이티브 상세 리뷰](assets/command-center-v2-review-20261020/native-review.md), [PWA 상세 리뷰](assets/command-center-v2-review-20261020/pwa-review.md), [실행/복구 상세 리뷰](assets/command-center-v2-review-20261020/workflow-review.md), [production/Relay 결과 JSON](assets/command-center-v2-review-20261020/production-relay-results.json), [drawer 포커스 JSON](assets/command-center-v2-review-20261020/drawer-focus-results.json). [증거 manifest](assets/command-center-v2-review-20261020/manifest.json)에 원 경로와 hash를 기록했다. 상세 리뷰의 /private/tmp 경로는 당시 전체 증거/독립 harness 위치이며 정리될 수 있다.

## 5. 실제 검증과 한계

- 현재 approved hash6/6 일치. root/서브에이전트가 source/control flow와 PNG/Chrome 동작을 검토했다. 기존 전체 suite의 PASS는 과거 사실이며 위15개 케이스의 정상 동작을 증명하지 않는다.
- 별도 workflow probe: 네 출력 시나리오 확인, exit0. 실제 reducer/queue/dispatch/SQLite 및 추출한 host/콜백 함수를 사용하며 App/fleet의 일부 서비스는 stub이다. 캐시는 합성 fixture다. 완료 후 broadcast 도달과 목표별 확인 소실을 입증하며 전체 앱 E2E 수가 아니다.
- 별도 runtime admission probe: production admission+guard+실제 소유 PTY 한 시나리오 exit0. 원래 앱/사용자 세션/실제 provider는 실행하지 않았다. root가 두 probe 실행을 반복 확인했다.
- 이번 native 독립 렌더: **1 PASS / 0 FAIL / 17 filtered**, PNG7장 생성. byte-identical UI 사본에 추가 test만 붙였고 popup은 실제 connector_ui 사용. 미사용 SensitiveInput 타입 shim이 있어 자격증명 동작의 증거로는 쓰지 않는다.
- Chrome:1440/1280/1000/760/430/390px, dark/light; shared font1/1.25. 참조12개·shared24개 고유 조합. 원시 metric48행은 고정 font 참조 중복을 포함한다. sampled 구성에서 document/app 가로 넘침 없음. composer 초안·한글 문자열·선택 위치는 일반 snapshot에 보존된다.
- 실제 production 자산의 순서·index·WebSocket 경로에서 체크박스 오류 확인. 실제 Relay 모듈·WebCrypto 암호화 fixture로 여섯 폭·보기 전용 상태를 확인하고 `RELAY_SHELL_OK`까지 실행했다. 네트워크는 격리된 로컬 fixture이며 실제 서명 server/기기간 연결 E2E를 반복한 것은 아니다.
- visualViewport420/layout844 모의에서는 composer가 화면 밖에 남는 기하 한계가 보였지만 **실제 휴대폰 키보드 결함으로 확정하지 않았다**. 실제 iPhone/Android 키보드·safe area, 사용자 DB 재시작, 유료 provider·물리 원격 기기, 장시간 CPU/memory/frame p95는 미검증이다.
- 처음 private rustc harness는 serde_core artifact 조합 불일치로 컴파일 실패했고 올바른 rlib 조합으로 해결했다. 첫 `--exact`는0 tests였으므로 PASS로 세지 않는다. PWA 사설 서버의 잘못된 SW fallback도 수정 후 격리 재실행했다. 이들은 제품 결함 수에 포함하지 않는다.

root가 실제 실행한 좁은 재현 명령:

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-command-center-20261020
/private/tmp/deppy-cc-review-workflow-20261020
/private/tmp/deppy-cc-review-admission-20261020
/private/tmp/deppy-cc-native-design-review-20261020/review-harness independent_design_review_extra_states --nocapture
git diff --check
```

probe 재생성 원문은 [workflow-probe.py](assets/command-center-v2-review-20261020/workflow-probe.py), [admission-probe.rs](assets/command-center-v2-review-20261020/admission-probe.rs)와 상세 리뷰의 rustc 명령에 있다. 당시 absolute source/target 경로·rlib hash에 의존한다. 영구 regression test로 제품에 추가한 것이 아니다.

## 6. 수정 작업을 나눈다면

1. 입력 수명/복원: F01, F02를 각각 독립 변경으로 고치고 저장 ACK→지연 콜백→결과 종료, 재시작·둘째 목표·설정 변경 시나리오를 추가한다. 자동 replay 금지 유지.
2. 작업 경로: F03, F06의 retry·종료 프로젝트 퇴출을 고친다. F07은 transport별 exact approval 지원을 별도 기능 단위로 연결한다.
3. PWA 갱신: F04, F05를 고치고 체크박스/일반 입력/IME와 drawer 갱신·Escape·복귀 포커스를 browser에서 재검사한다.
4. native 시안: F08–F12. 공통 치수·배치 규칙을 적용한 뒤 동일 크기·데이터에서 실제 egui PNG를 다시 비교한다. 외곽 containment만 확인하는 테스트로 대체하지 않는다.
5. shared 시안: F13–F15. PWA·Relay의 동일 정보 구조와 권한별 동작을 복원하고 탭 개수 대신 의미·정확한 세션 도착을 검증한다.

현재는 리뷰 요청에 따라 수정하지 않았다. 기존 화면 기본값·opt-in·rollback branch/tag를 유지한다. 이번 보고서는 기존의 ‘자동 검사 완료’를 **‘시안 일치·기능 인수 완료’로 해석하지 않도록 정정하는 검토 결과**다.
