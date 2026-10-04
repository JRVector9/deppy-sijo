# 전체 메모리·성능·에이전트 터미널 감사 — 2026-10-03

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| high | app.rs:29055, composer.rs:684 | 실제 입력 승인 전에 초안 소비, 본문과 Enter 별도 전송 | 입력 누락·기존 입력 제출·예약 소실 | 승인 추적과 실패 시 초안 복구 |
| high | ui/fleet.rs:607, fleet.rs:172 | AI 브로드캐스트 기본 대상에 일반 셸 포함 | AI 지시가 셸 명령으로 실행될 수 있음 | AI 실행 인스턴스 검증 |
| high | app.rs:14840, prompt_library.rs:44 | 손상·정상 빈 라이브러리를 기본 예제로 덮어씀 | 복구 가능한 프롬프트 데이터 손실 | 로드 상태 구분과 원본 보존 |
| medium | ui/composer.rs:251,385 | 초안별·전체 메모리 예산 없음 | 워크스페이스 수와 초안 크기에 따라 보존량 증가 | 초안 예산·저장·정리 정책 |
| medium | prompt_library.rs:69, ui/prompt_palette.rs:164 | 검색·미표시 행 작업 반복, 닫힌 팔레트도 초안 복사 | 긴 입력과 큰 라이브러리에서 반복 할당·프레임 지연 | revision/query 캐시·행 가상화·빌림 |
| medium | app.rs:28767, prompt_library.rs:55 | 프롬프트 저장을 UI에서 동기 수행 | 느린 디스크에서 입력 지연, 저장 실패 안내 부족 | 순서 보존 저장 작업자 |
| medium | ui/markdown_viewer.rs:185,432 | 이미지 제한 읽기 부재·동기 재읽기·문서 총량 미제한 | 상한 우회와 큰 이미지 작업 집합·편집 지연 | 핸들 제한 읽기·총량 예산·비동기 재사용 |
| medium | app.rs:24248, agent-mcp/history.rs:63 | MCP 이력 DB 작업이 UI 경로에서 대기 | 잠금·디스크 지연이 앱 응답을 막음 | claim 순서를 보존하는 DB 작업자 |
| medium | runtime/in_process.rs:3922, storage/logs.rs:109 | 로그 I/O와 tail 정리가 PTY 처리 루프에서 수행 | 느린 로그 디스크가 다른 세션에도 영향 | 길이 계수·배치부터 측정 |
| medium | app.rs:15467,15492 | 배치 실행 작업자 대기 중 즉시 repaint·프롬프트 복사 | 대기 시간 동안 불필요한 CPU·할당 | 완료 알림과 지연 재시도 |
| low | agent-mcp/src/lib.rs:19 | 원격 입력은 8KiB·개행/탭 불허 계약 | 긴 코드·여러 줄 지시를 보낼 수 없음 | 명시적 paste/submit 확장 |

## 검토 범위와 방법

- 현재 `0.5.5`, 기반 커밋 `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`와 작업 트리의 변경분을 함께 검토했다. 최근 팝업 변경만 검토한 것이 아니다.
- terminal/PTY/runtime의 소유권·캐시·채널·종료, App 렌더/작업자/파일 트리/문서, composer/library/broadcast/followup, Cloud MCP 입력·답변·이력 흐름을 확인했다.
- [리뷰 프롬프트](2026-10-03-full-agent-terminal-audit-prompt.md)를 먼저 작성하고 독립 Codex CLI 소스 리뷰를 실행했다. CLI는 읽기 전용으로 완료했고, 아래 재현과 측정은 별도로 수행했다. 협업 서브에이전트를 생성하지 않았다.
- 제품 소스는 수정하지 않았다. 실제 사용자 PTY에 프롬프트·명령을 넣지 않았다. 실제 App 시작/종료/재실행 없이 격리된 테스트와 함수 단위 측정을 사용했다.
- 테스트가 통과했다는 것은 **문제의 현재 동작을 확인하는 관찰 assertion이 맞았다**는 의미다. 수정 완료나 실제 GUI 전체 검증을 뜻하지 않는다.

## 1. 우선 수정할 입력·데이터 문제

### F1. 프롬프트 전달 결과와 초안 소비가 연결되지 않는다

근거: [composer.rs:671](../../crates/app/src/ui/composer.rs), [app.rs:28901](../../crates/app/src/app.rs), `app.rs:20430`, `app.rs:28966–29068`, [in_process.rs:2223](../../crates/runtime/src/in_process.rs).

`try_submit`은 `mem::take`로 초안을 비우고 이력을 갱신한 뒤 Send intent를 반환한다. App의 단일 staging 슬롯이 이미 차 있거나 대상이 없어져 전달을 생략해도 복원 경로가 없다. 이력에서 회수할 수 있는 경우가 있으므로 모든 경우의 영구 유실이라고 표현하지는 않는다. staging 거절 시 새 이력의 영속화도 연결되지 않는다.

Bracketed paste 본문과 submit CR은 별도의 untracked `WriteInput`이다. 명령 큐의 수용과 실제 PTY 큐의 수용은 다른 단계이며, runtime은 untracked 입력의 `admit_input` 결과를 버린다. PTY 큐 자체는 payload 하나를 전체 수용한다. 문제는 두 payload가 하나의 사용자 동작으로 연결되지 않는다는 것이다.

실제 기본 정책 4MiB/256메시지로 다음 두 경우를 재현했다.

1. 큐 잔여 8B에서 본문은 backpressure, 1B CR은 수용됐다. 실제 TUI의 기존 초안이 있다면 그 초안을 제출할 수 있다.
2. 기존 대기 입력과 1MiB 프롬프트 본문이 큐를 채우면 본문은 수용되고 CR은 backpressure였다.

`flush_queued_followups`는 전송 전에 예약을 지우고, broadcast는 수용 결과와 무관하게 낙관적 작업 중 표시를 넣는다. 같은 승인 문제를 공유한다.

**수정 방향:** 제출 ID·원래 runtime/session/AI 실행 인스턴스를 유지하고, 전체 paste/submit에 대한 입력 수용 결과를 추적한다. 명확한 무효과 거절은 초안·예약을 복구한다. 결과 불명 상태에서는 자동 재전송하지 않는다. 이미 Cloud MCP에 있는 `WriteInputTracked`/`InputAdmission` 구조를 재사용할 수 있으나, 큐 승인과 AI 실행/완료는 계속 구분해야 한다. 단순히 body+CR을 합칠 때는 provider별 TUI 제출 동작도 검증해야 한다.

### F2. AI 브로드캐스트가 일반 셸에 지시를 제출할 수 있다

근거: [fleet.rs:172](../../crates/app/src/fleet.rs), [ui/fleet.rs:607](../../crates/app/src/ui/fleet.rs), `app.rs:28518`, `app.rs:32401`, [agent_launcher.rs:199](../../crates/app/src/agent_launcher.rs).

`broadcast_key`는 모든 PTY를 허용한다. Fleet에는 status가 있는 일반 셸도 들어오고, 브로드캐스트의 기본 선택은 `Active`가 아닌 PTY다. AI 실행 여부를 요구하지 않는다. 대상 선택 화면의 라벨도 제목·워크스페이스·상태이며 AI 실행 인스턴스의 유효성을 보장하지 않는다.

AI 종료 후 같은 PTY에서 fallback 셸이 이어진다. 실제 전송 시 AI 생존 확인이 없어 이 셸에 프롬프트+Enter를 보낼 수 있다. 유효한 셸 문법의 프롬프트라면 명령으로 실행될 수 있다. 소스 경로는 확인했으나 사용자 셸에서 실행하는 재현은 하지 않았다. **확인된 것은 자동으로 몰래 실행한다는 결함이 아니라, 사용자가 브로드캐스트를 실행할 때 기본 대상의 자격이 부정확하다는 점이다.**

**수정 방향:** 살아 있는 AI 호출만 기본 대상으로 하고 실제 수용 직전에 실행 인스턴스·generation을 재검증한다. AI 종료 시 예약/자동 제출 자격을 해제한다. 셸 전송은 사용자에게 별도 대상으로 명확히 표시한다. 이미 대상 PTY에 편집 중인 초안이나 dialog가 있는 경우에는 덮어쓰지 않는 guard도 필요하다.

### F3. 프롬프트 라이브러리 시작 처리에서 원본을 덮어쓴다

근거: [prompt_library.rs:44](../../crates/app/src/prompt_library.rs), `app.rs:14839–14846`.

로드 실패·파일 없음·정상 빈 목록이 모두 `default()`로 합쳐진다. App은 빈 목록이면 기본 예제를 만들고 즉시 저장한다. 격리된 손상 JSON과 정상 `{"prompts":[]}`에 실제 load/save와 동일한 App 시작 분기를 적용했더니 모두 예제 4개로 교체됐다. 원본 백업은 없었다.

따라서 사용자가 일부 복구할 수 있는 손상 파일을 잃고, 모든 프롬프트를 지운 사용자도 다음 시작 때 예제를 다시 받는다. atomic rename은 쓰기 중 부분 파일을 방지하지만 이 의미상의 덮어쓰기를 막지는 않는다.

**수정 방향:** Missing/Loaded/Error를 구분하고 Missing에서만 seed한다. 정상 빈 목록은 유지한다. 손상/읽기 실패는 원본을 보존하고 사용자에게 복구·백업 경로를 제공한다. 저장 실패도 로그만 남기는 대신 상태를 표시한다.

## 2. 메모리와 화면 작업

### F4. 초안의 메모리 예산과 생명주기가 부족하다

`ComposerUi.buffers`는 workspace별 `String`을 소유한다. 1MiB 상한은 전송 시 검사하며 입력·`insert_text` 저장량을 제한하지 않는다. 실제 Composer에 2MiB 초안 하나와 1MiB 초안 32개를 넣어 **33엔트리, 텍스트 35,651,584B(34MiB)** 보존을 확인했다. HashMap/문자열 capacity/egui 상태는 이 수치에 포함하지 않았다.

workspace close는 숨김·복구를 위해 초안을 보존하는 동작이므로 누수로 단정하지 않는다. 현재 App에서 직접 `delete_workspace`를 호출하는 경로는 benchmark 삭제로 확인됐다. 영구 삭제/외부 DB 갱신 뒤 composer ID 정리가 없는 것은 수명 관리 공백이지만, **일반 사용자의 종료를 반복할 때 도달 불가능한 메모리가 누적된다는 전체 GUI 재현은 하지 않았다.** 독립 리뷰의 영구 삭제 누수 표현을 이 범위로 좁혔다.

**수정 방향:** 세션별 초안과 전체 byte 예산, 디스크 체크포인트, 확정 삭제 시 정리를 설계한다. 닫은 유효 초안을 임의로 버리지 않는다. 일부 목록만 읽는 bounded projection을 전체 DB라고 간주해 prune하는 방식도 피한다.

### F5. 프롬프트 검색·렌더·닫힌 팔레트에서 불필요한 작업

- `app.rs:28875`는 기능이 켜져 있으면 매 프레임 전체 draft를 복사한다. 기본 설정은 켜져 있고 팔레트는 닫혀 있으면 즉시 반환한다. `&str`로 빌리거나 열림 상태에서만 작업할 수 있다.
- `PromptLibrary::search`는 nonempty query에서 제목/본문/태그를 매번 소문자 문자열로 만든다. query와 library가 그대로여도 매 렌더 반복한다.
- palette 목록은 `ScrollArea::show`에서 모든 검색 결과의 위젯을 만든다. 표시되는 행만 그리는 `show_rows` 방식이 아니다.
- detail의 params와 치환 preview도 그대로인 상태에서 재계산한다.

**우선 순서:** revision/query가 변할 때만 검색 → 표시 행 가상화 → draft 빌림 → 선택 prompt/parameter 변경 시 preview 갱신. 전체 본문을 소문자로 중복 보관하는 방식은 속도와 메모리의 교환이므로 기본 해결책으로 권하지 않는다.

### F6. 프롬프트 라이브러리의 UI 동기 저장

`render_composer_dock`의 Upsert/Delete에서 `persist_prompt_library` → pretty JSON 전체 직렬화 → 임시 파일 쓰기 → rename을 UI 경로에서 실행한다. load도 무제한 `read_to_string`이다. 현재 로컬 디스크 측정은 아래와 같고 느린 디스크·파일 시스템의 최대 지연은 측정하지 않았다.

**수정 방향:** 입력 파일/항목/본문 총량을 제한하고, App이 소유하는 bounded/coalescing 저장 작업자로 옮긴다. revision과 저장 완료/실패 상태를 유지한다. atomic rename을 보존하고 이전 revision 완료가 새 변경을 덮어쓰지 않도록 한다.

### F7. Markdown 이미지 읽기 상한과 작업 집합

`validate_and_read`는 stat에서 8MiB를 확인한 후 `fs::read`로 제한 없이 읽는다. 격리된 원본 함수에 **stat 직후 파일을 늘리는 테스트 전용 hook**을 넣었더니 유효 PNG가 **8,388,609B**로 반환됐다. limit+1 한 바이트의 재현은 안전하게 수행했고, 읽기가 실제 상한으로 제한되지 않는다는 점을 증명한다.

image broker는 문서 revision이 바뀌면 기존 이미지를 해제하고 모든 image 참조를 UI에서 canonicalize/metadata/read한다. 텍스트만 고쳐도 이미지가 다시 읽힌다. 개별 이미지 치수/픽셀 제한은 있으나 문서 전체 encoded bytes/decoded pixel 예산은 없다. 보이지 않는 이미지도 먼저 bytes를 등록한다. 문서 닫기·revision 교체의 해제 경로는 있으므로 이 항목을 종료 후 이미지 누수로 분류하지 않는다.

**수정 방향:** 검증한 핸들에서 limit+1 제한 읽기, 문서 총량과 decoded pixel 예산, bounded worker와 slot/revision 완료 검증, 파일 identity/변경 기준 재사용, 표시 영역 중심 로드를 적용한다. 50개 이미지 문서의 GUI 지연/texture memory는 후속 실측 항목이다.

## 3. 처리 루프의 지연

### F8. Cloud MCP 이력 DB가 UI를 기다리게 한다

App의 `pump_cloud_agent`는 최대 16개 요청을 한 프레임에서 처리한다. `handle`의 durable claim/finish/history reload가 동기 SQLite다. 실제 `History::claim`에 다른 연결의 exclusive lock을 걸었더니 오류 반환까지 **127.305ms** 걸렸다. 설정 busy timeout은 100ms이며 스케줄링까지 포함한 실제 경과가 더 길었다. 이것은 DB 함수 측정이고 실제 GUI 한 프레임이나 16개 요청의 합산 측정은 아니다.

**수정 방향:** 입력 effect보다 claim 영속화를 먼저 완료하는 순서를 지키며 DB 작업자로 분리한다. 결과가 돌아오면 token/deadline/grant/runtime/AI 실행 인스턴스를 다시 검사한다. 늦은 결과를 곧바로 입력으로 연결해서는 안 된다. 기존 at-most-once tombstone과 unknown 결과의 재시도 금지도 유지한다.

### F9. 로그 I/O가 PTY 처리와 같은 루프에 있다

runtime은 각 session `pump`에서 redaction 후 ANSI/plain 로그를 동기 기록한다. append마다 file metadata를 조회하고 상한 도달 시 tail 정리를 수행한다. 디스크가 느리면 같은 runtime의 다른 PTY 처리와 snapshot이 늦어질 수 있다. 로그 byte 상한은 이미 존재하며 무제한 파일 누수를 발견한 것은 아니다.

**수정 순서:** 먼저 writer-owned 길이 계수와 제한된 배치 쓰기를 측정한다. 그다음 필요할 때 bounded logging worker를 고려한다. redaction 선행·세션별 순서·종료 flush·큐 압력/디스크 오류 정책을 유지해야 한다. 현재 감사에서는 느린 writer와 실제 두 PTY의 입력 지연을 측정하지 않았다.

### F10. 배치 스폰 대기 중 프레임을 계속 요청한다

`pump_batch_spawn`은 pending prompt를 매 tick 복제한다. settings 작업자가 바빠 queue가 거절되어도 pending이 있으면 즉시 repaint한다. worker 완료 알림으로도 진행할 수 있는 동안 계속 렌더·복사가 발생한다.

**수정 방향:** 진행됐을 때만 다음 프레임을 즉시 요청한다. busy 상태는 완료 wake 또는 제한된 지연 재시도로 처리하고 프롬프트는 공유한다. 현재 워크스페이스 전환 취소 규칙을 보존한다. 2초 busy fixture의 실제 frame count는 미측정이다.

낮은 우선순위로 exit sentinel polling도 있다. 매 runtime pump에서 AI별 없는 파일을 `read_to_string`으로 확인한다. 32회 한 묶음은 **29.477μs, heap 할당 0B**였다. 현재 측정으로는 검색/DB보다 비용이 작다. 주기를 분리할 경우 종료 감지 지연과 메타데이터 syscalls를 함께 비교한다. 앱 전체 CPU 절감률로 환산하지 않았다.

## 4. 에이전트 터미널로 사용할 때의 부족한 부분

| 흐름 | 현재 구현 | 필요한 개선 |
|---|---|---|
| 초안 작성 | workspace별 draft, 이력, 첨부/파일 @mention, 파라미터 prompt library | 세션별 draft·재시작 복구·전체 예산 |
| 전송 대상 | runtime/session 및 attached 관계를 검사 | 대상 세션/AI/model/추론 표시와 AI→셸 전환 안내 |
| 제출 | 로컬 provider별 paste 계획 | 수용 상태·실패 복구·기존 TUI draft/dialog guard |
| 여러 대상 | broadcast·예약 후속·batch spawn | 대상별 성공/거절/결과 불명 상태와 실행 인스턴스 확인 |
| 원격 입력 | 8KiB, control/newline/tab 불허, submit CR | explicit multiline paste/submit와 provider 공용 입력 경로 |
| 원격 출력 | 최대64KiB 현 화면, cursor는 화면 변경 버전 | 필요 시 턴/로그 범위 읽기·완료 대기·truncation 안내 |
| 클라우드 자체 답변 | notify로 원래 세션의 이력/알림에 보관 | 전송 수용과 AI 완료·최종 답변을 UI에서 구분 |

원격의 여러 줄 거절은 명시된 계약이며 버그가 아니다. 실제 encoder에서 한글 8,190B는 수용, 8,193B는 거절됐고 개행·탭은 거절됐다. raw LF 허용만 하면 셸에서 여러 명령을 실행할 수 있으므로 paste 계약으로 확장해야 한다.

`read_output`은 스스로 `lossless:false`, `may_be_stale:true`라고 알린다. cursor가 stdout의 연속 offset이 아니라 화면 버전이므로 화면 밖으로 지난 전체 응답 수신을 보장하지 않는다. 지금부터 새로운 백그라운드 AI를 돌려 해결할 필요는 없다. **사용자가 보고 있는 같은 세션**의 redacted 로그/지원되는 턴 경계에서 bounded 읽기나 완료 대기를 제공하는 방향이다. 이 기능은 현재 지원 provider별 실제 출력 형식을 검증한 뒤 확장해야 한다.

## 5. 이번에 실행한 측정

환경: Apple M5 Max, macOS 27.0, rustc 1.96.1. 별도 release harness가 현재 `PromptLibrary` 원본 모듈을 import했다. System allocator로 누적 할당을 세었다. 실제 App의 allocator/RSS/GPU 및 end-to-end frame time과 다르다. 5개 샘플의 중앙값이다. fixture 구성은 측정 밖에서 했다.

| 항목 | 현재 측정 | 제한/해석 |
|---|---|---|
| 약1MiB draft 복사 | 12.050μs,1,048,575B/회 | repaint마다 수행되면 반복 할당; 실제 FPS 미측정 |
| 같은 draft 빌림 제안 | 0B 할당 | 0ns 표시는 타이머 해상도 아래; 물리적 비용0이라는 뜻 아님 |
| 100×본문8,000B 검색 miss | 1.604ms,801,501B/회 | nonempty miss query; 위젯 렌더 비용 제외 |
| 1,000×본문8,000B 검색 miss | 16.221ms,8,015,901B/회 | 그 자체가60Hz frame budget약16.7ms에 근접 |
| 1,000개 normalized 검색 prototype | 0.222ms,0B/회 | 본문 cache8MB추가; 제품에 적용하지 않음 |
| 100개 전체 저장 | 0.381ms,2,061,645B 누적 할당 | local temp disk의 serialization+write+rename |
| 1,000개 전체 저장 | 3.480ms,16,548,173B 누적 할당 | 느린 디스크 최대값 아님 |
| 32개 없는 sentinel 읽기 | 29.477μs,0B | syscall 함수 측정; runtime 전체 비용 아님 |
| 잠긴 MCP DB claim | 127.305ms 한 번 | 실제 History/SQLite, GUI frame 측정 아님 |
| 33 workspace 초안 | 34MiB 텍스트 보존 | capacity·map·egui 비용 제외; unreachable leak 재현 아님 |

normalized prototype은 영어/한글/빈 query 5종에서 실제 검색과 결과가 일치했다. 이 제한된 동등성 검증을 전체 Unicode 검색 증명으로 확대하지 않는다. 경량화 목표에서는 우선 **revision/query result 재사용**으로 반복 자체를 제거하고, 본문 중복 cache는 필요할 때 예산 안에서 채택한다.

8개 격리 관찰 테스트: App4개(0.01s), 기본 PTY 정책2개(0.00s), MCP2개(0.13s), 모두 pass. fixture를 통해 입증한 경계와 정적 소스만 확인한 경계를 위에서 구분했다. 기존 전체 App 테스트 결과는 이전 작업의 기록이며 이번에는 전체 스위트를 재실행하지 않았다.

## 6. 다른 프로젝트와 비교

검색 결과의 설명 대신 원본 코드/공식 문서를 확인했다. GitHub branch source를 HTTP로 읽고 관찰 당시 commit도 기록했다. 원문 proof JSON이 잘린 경우 읽은 앞부분의 주장에만 사용했다.

| 프로젝트 | 원본에서 확인한 방식 | Deppy에 적용할 부분 |
|---|---|---|
| cmux | paste/submit 구분, agent-aware submit, 기존 agent draft/dialog guard, 정확한 surface 대상 | F1/F2의 입력 공용 서비스와 대상 guard |
| Alacritty | 이벤트 기반 PTY loop, 읽기/lock 점유량 제한, synchronized update timeout | 기존 wake/frame pacing 유지; 로그 I/O와 대기 루프 분리 검토 |
| Ghostty | page 최대 bytes/lines, viewport 최소 확보, demand-paged pool, owned decode buffer 해제 | 이미 있는 압축·예산 유지; 이미지/초안에도 명확한 총량 예산 |
| WezTerm | animation 없으면 invalidation 빈도 축소, atlas 부족 시 clear/grow와 image 축소/비활성 fallback | batch idle repaint와 이미지 budget/fallback 정책 |
| Warp | 확장 입력 editor, 코드 선택 context, review comment 전달, agent metadata | 세션별 초안과 긴 코드 지시 작성·대상 표시 UX |

근거: [cmux CLI contract](https://github.com/manaflow-ai/cmux/blob/main/docs/cli-contract.md), [Alacritty event loop](https://github.com/alacritty/alacritty/blob/master/alacritty_terminal/src/event_loop.rs), [Ghostty PageList](https://github.com/ghostty-org/ghostty/blob/main/src/terminal/PageList.zig), [WezTerm paint](https://github.com/wezterm/wezterm/blob/main/wezterm-gui/src/termwindow/render/paint.rs), [Warp 공식 Claude Code 가이드](https://docs.warp.dev/agents/cli-agents/claude-code).

관찰 commit: cmux `ef96c2ec`, Alacritty `d692748d`, Ghostty `822e8427`, WezTerm `cab25161`. Warp는 이번 비교에서 공개 runtime 내부 코드가 아니라 공식 기능 문서만 근거로 사용했다. 이 문서로 Warp의 메모리 구현이 우수하다고 단정하지 않았다.

기존 7개 개선(압축 history accounting/scratch, grapheme, CJK run, IME 소유권, compact cell, shared dirty rows)은 현재 소스에 있다. 재구현하거나 terminal backend를 전면 교체하는 것보다 위 입력/I/O/반복 계산 개선이 먼저다. 다른 프로젝트의 page pool을 그대로 도입하면 복잡성과 보존 메모리가 늘 수 있으므로 필요성을 실측해야 한다.

## 7. PR 단위 권장 순서

| PR | 범위 | 완료 확인 |
|---|---|---|
| 1 | 공용 prompt 전송 수용 추적·초안/후속 복구 | body/CR 각 거절, staging busy, stale target, unknown 재전송 금지 |
| 2 | broadcast/예약의 AI 실행 자격과 입력 draft/dialog guard | 일반 셸 기본 제외, AI 종료 경합, 실행 인스턴스 교체 |
| 3 | prompt library 복구·빈 목록 유지·bounded 비동기 저장 | 손상 원본 보존, 저장 실패 UI, revision 순서 |
| 4 | palette result/preview 캐시·가상 행·draft 빌림 | 위 측정 재실행, 검색 동일성, 표시 행 수·cache 총량 |
| 5 | session별 draft/checkpoint/전체 byte 예산 | 전환·재시작 복구, 확정 삭제 정리, 숨김 초안 보존 |
| 6 | Markdown 제한 읽기·image worker/총량/재사용 | 파일 증가 경합,50이미지 편집, 닫기·늦은 완료·texture 회수 |
| 7 | MCP DB 작업자와 프레임 시간 예산 | lock fixture 중 UI 진행, claim-before-input·만료·철회 |
| 8 | batch wake 개선과 runtime 로그 배치 계측 | busy frame count,2PTY 입력 지연,redaction/순서/종료 flush |
| 9 | 공용 sender를 활용한 remote paste/submit·필요 시 turn read/wait | 여러 줄 한글/코드,provider 제출,원래 visible session·bounded 결과 |

PR1–3은 전달/데이터 신뢰성, PR4/6/7은 체감 속도의 우선 후보다. PR8의 로그 thread 분리는 먼저 길이 계수/배치 효과를 확인한 뒤 결정한다. PR9는 명시적인 API 변화이므로 기존 단일 행 계약을 유지하며 도구를 추가하는 방식을 검토한다.

## 8. 누수 의심을 제외한 항목과 한계

- CJK 폰트 `Box::leak`은 `OnceLock` 안에서 한 번만 생성된다. 프로세스 수명 고정 cache이며 변경할 때마다 쌓이는 누수로 확인하지 않았다. native monitor/source의 영구 보관도 일회 설치 guard가 있다.
- PTY queue, runtime command/event budget, subscriber 제거, remote view lease 만료, hidden/archived terminal budget이 있다. 모든 HashMap을 무제한 누수로 처리하지 않았다.
- renderer는 dirty row 공유와 bounded row/scaled cache 및 hidden/exit clear를 사용한다.
- 파일 트리는 worker listing, immutable snapshot, visible row 렌더와 retained/watch 예산이 있다. 문서 본문에도 tab/byte 제한과 비동기 identity 검증이 있다.
- Cloud MCP는 원래 session UUID/generation/runtime/pane 관계와 grant/deadline, durable operation ID, tracked input 수용을 사용한다. notify는 stdin 실행이 아니라 원래 세션의 답변/알림이다. 별도 보이지 않는 AI 프로세스를 생성하는 경로를 이 bridge에서 발견하지 않았다.
- Rust Arc cycle 또는 native 객체 누적 누수를 입증하지 못했다. 이는 앱 전체가 누수 없다는 보증이 아니다. 실제 장시간 RSS/GPU plateau, provider TUI, SSH reconnect, 느린 로그 디스크의 전체 GUI 측정은 수행하지 않았다.

## 재현 명령과 증거

제품 파일 대신 현재 변경분의 격리 복사 `/private/tmp/deppy-popup-check-review-20261003`에만 review test/hook을 넣었다. 세션은 실제 사용자 세션과 연결하지 않았다.

```sh
cd /private/tmp/deppy-popup-check-review-20261003
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo full_audit_probe -- --test-threads=1 --nocapture
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p pty full_audit_probe -- --test-threads=1 --nocapture
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p agent-mcp full_audit_probe -- --test-threads=1 --nocapture
cargo run --offline --locked --release --manifest-path /private/tmp/deppy-full-audit-bench-20261003/Cargo.toml
```

- 로그: `/tmp/deppy-full-audit-{app-final-probes,pty-default-probes,mcp-probes,bench-final}-20261003.log`.
- CLI 결과: `/tmp/deppy-full-agent-terminal-audit-cli-result-20261003.txt`; 입력 프롬프트와 전체 로그도 같은 prefix에 보존.
- 원문: `/tmp/deppy-full-audit-{cmux,alacritty,wezterm,warp}-proof-20261003.json`, `ghostty-correct-proof`, `research-commits`.
- 실패 접근: Ghostty 소문자 `page_list.zig`는404여서 근거에서 제외하고 API의 `PageList.zig`를 확인했다. benchmark 최초 edition2021은 실제 모듈의2024let-chain 때문에 compile 실패했으며 harness만2024로 변경해 재실행 성공했다. CLI sandbox에서 heredoc 임시 파일 생성 실패는 제품 오류로 세지 않았다.
- 기존 release scope 소스 hash는 `891a6a2312db9c90172fbce2bd3e664c140b48157f0a156043de9f6fd84ca847`로 동일하다. `/tmp/deppy-full-audit-source-integrity-20261003.json`에 확인 기록. 문서만 추가/갱신했으므로 제품 버전0.5.5 변경이나 새 제품 빌드를 배포하지 않았다.
