# 키 입력 지연(input latency) 조사 (2026-08-18)

작성일: 2026-08-18
브랜치: `perf/input-latency` (`main` 84978dc 기준)
성격: **정적 감사 + 실측(마이크로벤치/실제 PTY 왕복)**. GUI 앱은 빌드·실행하지 않았다
(오케스트레이터 지시). `logic()`/`ui()` 전체 프레임 시간은 GUI가 없으면 잴 수 없어 미검증으로
남긴다 — 아래 8장 참고.

## 0. 요약 (TL;DR)

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| 상 | `crates/pty/src/lib.rs` `unix_reader_loop`/`PTY_COALESCE_WAIT_MS` | 고립된 PTY 출력(키 입력 echo 1개)마다 코얼레싱 대기를 **매번 타임아웃까지 그대로 지불**했다. 실측 중앙값 2.28ms/echo | 키 입력 후 화면 echo가 보이기까지의 왕복에 **echo당 ~2.3ms**가 항상 얹힘 | **수정 완료**: `PTY_COALESCE_WAIT_MS` 2ms→1ms. 중앙값 1.19ms로 48%↓, 대량 출력 코얼레싱은 그대로(청크 4개, 이전 2개) |
| 정보 | `logic()`의 poll_*/pump_* 함수 30개 전수 | 전부 `try_recv`/`Option::take`/플래그 조기 반환으로 게이트됨. 무조건 매 프레임 실행되는 무거운 계산 없음(2026-08-14 i18n clone 버그는 이미 수정됨, 재확인) | 없음 — 병목 아님 | 코드 변경 없음 |
| 정보 | `RuntimeCommand::WriteInput` 생성→PTY write 경로 | `queue_protocol_intent`(App 소유, 수정 금지 영역)→`drain_workspace_protocol_intents`→`send_command`→`unpark`→writer thread `libc::write` 전 구간 **인위적 지연 없음**(unpark 기반 즉시 전파) | 없음 — 병목 아님 | 코드 변경 없음 |
| 낮음(미수정) | `crates/runtime/src/client.rs` `RuntimeEventReceiver::drain()` | 이벤트가 하나도 없어도 `Vec::with_capacity(256)`을 무조건 할당(측정 ~20ns, 총 44.5ns/call) | 프레임 예산(16ms) 대비 6자리 이상 작음 — **체감 불가** | 수정하지 않음(근거 없는 최적화 방지) |
| 정보(내 소유 밖) | `App::logic()` → `ui()` 사이의 intent-queue 구조 | leaf(ui())가 WriteInput을 큐에 올리고 App(logic())이 다음 프레임에 드레인 — 구조상 **최소 1프레임(vsync 1~2회)** 지연이 불가피 | 60Hz 기준 최대 ~16.6ms, 아키텍처 설계(다른 에이전트 소유, 수정 금지) | 보고만 함 |

## 1. 조사 범위와 방법

과제가 제시한 4갈래 분해를 그대로 따라 검증했다:

- (a) 프레임 자체가 느린가 — `App::logic()`의 poll_*/pump_* 30개 함수를 전수 읽었다.
- (b) repaint 스케줄링이 늦는가 — `flush_command_repaint()` 호출 지점을 추적했다.
- (c) 키 → PTY write 사이에 인위적 지연이 있는가 — `queue_protocol_intent`(읽기만)부터
  `crates/runtime`, `crates/pty`의 실제 코드 경로를 끝까지 따라갔다.
- (d) PTY 출력 → 스냅샷 → 화면 반영이 느려서 입력이 느린 것처럼 보이는가 — **실제로 여기서
  병목을 찾았다** (아래 4장).

GUI를 띄우지 않고 실측하기 위해 두 가지 방법을 썼다:
1. `crates/pty` crate 자체에 임시 `#[ignore]` 마이크로벤치를 추가해 실제 PTY(`/bin/cat`,
   `/bin/sh`)를 spawn하고 `write_input()` → 출력 채널 도착까지의 벽시계 시간을 쟀다. 결론이
   난 뒤 이 벤치들은 정식 회귀 테스트로 바꿨다(§6).
2. `crates/runtime/src/client.rs`에 임시 마이크로벤치를 추가해 `RuntimeEventReceiver::drain()`의
   alloc 비용을 쟀다(§5). 측정 후 코드 변경 없이 원복했다.

`crates/app/src/bench.rs`(B1 시나리오 드라이버)와 `cargo run -p xtask -- perf-smoke`는 실제로
확인했다 — 둘 다 GUI(전체 App 인스턴스 + eframe::Frame)가 있어야 값을 내는 구조라 이번 조사
방식(GUI 미실행)에는 쓸 수 없었다(`perf-smoke`는 구조적 스모크 테스트 16개일 뿐 타이밍 측정이
아니다). `App::logic()`을 직접 호출하는 유닛 테스트도 없다(`eframe::Frame`을 만드는 유일한
테스트 경로 `Frame::_new_kittest()`로 App 전체를 구성하는 하네스가 존재하지 않음) — 그래서
`logic()` 전체의 프레임당 벽시계 시간은 정적 감사(전수 읽기)로만 판정했다.

## 2. (a) `logic()`의 poll_*/pump_* 30개 전수 감사

`crates/app/src/app.rs:22548`의 `fn logic()`이 순서대로 부르는 함수 전부를 읽었다:
`flush_queued_pty_adjustments`, `handle_configured_shortcut`, `poll_agent_launcher_detection`,
`poll_dotenv_sync`, `poll_agent_state_worker`, `poll_work_history_git`,
`pump_workspace_restore_delivery`, `pump_startup_deferred_dotenv_continuations`,
`auto_scan_ports_once`, `poll_port_inventory`, `pump_resource_maintenance`, `pump_perf_harness`,
`pump_batch_spawn`, `poll_env_secret_reveals`, `poll_env_secret_reveal_admission`,
`poll_settings_outcomes`, `poll_settings_job_admission`, `poll_env_api_project_rows`,
`poll_file_tree_maintenance`, `poll_app_host_io`, `poll_app_controller`, `poll_workspace_controller`,
`poll_pending_workspace_focus`, `poll_turn_done_clear`, `apply_pending_visual_settings`,
`poll_worktree_jobs`, `refresh_agent_workspace_cwd`, `pump_notice_translations`,
`pump_ollama_detect`, `poll_approval_wake`, warm/active workspace event drain 블록,
`poll_workspace_protocol_intents`(WriteInput 드레인 지점 — §3).

**판정: 전부 조기 반환 게이트가 있다.** 패턴은 셋 중 하나다.

1. `while let Ok(x) = channel.try_recv() { ... }` — 채널이 비면 즉시 반환 (`poll_dotenv_sync`,
   `poll_agent_state_worker`, `poll_settings_outcomes`, `poll_env_secret_reveals`,
   `poll_port_inventory`, `pump_notice_translations` 등 대다수).
2. `let Some(x) = self.pending_x.take() else { return; }` — 대기 작업이 없으면 즉시 반환
   (`poll_workspace_controller`, `pump_workspace_restore_delivery`, `pump_batch_spawn`,
   `pump_resource_maintenance`, `poll_turn_done_clear` 등).
3. 플래그/필드 비교 후 조기 반환 (`auto_scan_ports_once`의 `port_auto_scan_done`,
   `refresh_agent_workspace_cwd`의 `agent_workspace_cwd_key` 동일성 비교,
   `apply_pending_visual_settings`의 테마/폰트/스케일 비교).

2026-08-14 CPU 조사 문서(`docs/performance/cpu-investigation.md`)가 지적했던
"`poll_worktree_jobs`가 매 프레임 `i18n::Catalog` 전체를 clone" 버그는 **이미 수정돼 있음을
재확인**했다 — 현재 코드(`app.rs:15476-15481`)는 두 채널이 비어 있으면 `self.i18n`을 전혀
건드리지 않는다는 주석과 함께 조건부 대여로 바뀌어 있다.

새로 발견한 무조건 실행 코드는 `refresh_agent_workspace_cwd`(`self.workspaces.iter().find(...)`
— 매 프레임 워크스페이스 목록 선형 탐색)와 `pump_notice_translations`(rx가 비어 있어도
locale 지원 확인 후 상태 feed incident 목록을 매 프레임 필터링)였다. 둘 다 대상 컬렉션이
작다(워크스페이스 수십 개, incident 0~수 개 수준)는 것을 코드로 확인했고, 문자열 비교/필터
수준이라 **마이크로초 미만**으로 판단해 별도 실측 없이 병목 후보에서 제외했다.

**결론: `logic()`의 poll/pump 목록 자체에서 "프레임당 비용이 큰 것"은 찾지 못했다.**

## 3. (b) repaint 스케줄링 — 키 입력 후 즉시 걸리는지

`crates/app/src/ui/workspace.rs`의 `show_with_input()`(:3374)이 `render_node()`(입력 캡처 +
`WriteInput` 큐잉이 일어나는 지점)를 호출한 **직후** `self.flush_command_repaint(ui.ctx())`를
호출한다(:3496). `flush_command_repaint()`(:5848)는 이번 프레임에 `queue_protocol_intent`가
호출돼 `command_sent` 플래그가 섰으면 `ctx.request_repaint()`를 건다. 즉 키 입력을 소비해
intent를 큐에 올린 **바로 그 프레임 안에서** 다음 프레임 repaint를 예약한다 — idle 타임아웃을
기다리지 않는다.

**판정: repaint 스케줄링은 정상이다.** 개선할 부분을 찾지 못했다.

## 4. (c) 키 → PTY write 경로의 인위적 지연 — 없음

`send()`(workspace.rs:6148) → `queue_protocol_intent`(App 소유 영역, 읽기만 함) →
(다음 프레임 `logic()`의) `poll_workspace_protocol_intents` → `drain_workspace_protocol_intents`
→ `self.active.runtime.send_command(command)`까지 추적했다. 이 지점부터는 내 소유
(`crates/runtime`, `crates/pty`)라 끝까지 코드를 읽었다:

1. `InProcessRuntimeClient::send_command`(`in_process.rs:381`) — `tx.try_send(queued)` 직후
   **동기적으로** `worker_thread.unpark()`를 호출한다. 대기·배칭 없음.
2. runtime worker 스레드의 `run()` 루프(`in_process.rs:1194`)는 `park_timeout(wait)`으로
   자고 있다가 위 `unpark()`로 **즉시** 깨어나 `command_rx.try_recv()`로 명령을 소비한다.
3. `RuntimeCommand::WriteInput` 처리(`in_process.rs:1736`)는 `active.write_input(&bytes)` →
   `pty::input_queue::enqueue_input`(`crates/pty/src/input_queue.rs:135`)을 호출한다. 이 함수는
   **락을 잡고 즉시 반환**하며(`try_send`만 사용, blocking write는 별도 writer 스레드 몫이라는
   불변식이 doc comment에 명시돼 있다), PTY 입력 쓰기 전용 writer 스레드의 채널로
   `tx.try_send(chunk)`한다.
4. writer 스레드(`unix_writer_loop`, `crates/pty/src/lib.rs:1278`)는 `for bytes in &input`으로
   그 채널을 **블로킹 recv**하며 대기하다가, 청크가 도착하면 즉시 `poll(POLLOUT)` 후
   `libc::write()` 시스템콜을 수행한다. 배칭/슬립 없음.

**판정: 이 구간 전체에서 인위적 지연/폴링 간격/라운드트립을 하나도 찾지 못했다.** 매 hop이
`unpark`/blocking-recv 기반으로 즉시 전파된다. `docs/performance/cpu-investigation.md`가 언급한
`output_batch_ms`/`ACTIVE_VIEWPORT_FRAME_INTERVAL(8ms)`는 **출력 뷰포트 스냅샷 push 페이싱**이지
입력 write 경로와는 무관함을 코드로 재확인했다.

## 5. (d) PTY 출력 → 스냅샷 → 화면 반영 — 여기서 실제 병목을 찾았다

### 5.1 가설과 근거

`crates/pty/src/lib.rs`의 `unix_reader_loop`(macOS PTY read를 ~1KB로 캡하는 커널 제약을
우회하려고 2026-07-24 커밋 `c4e9ab3`에서 도입된 코얼레싱 로직, 대량 출력 청크를 최대 128KiB로
합쳐 채널 send/wake 폭주를 막는다)는, 논블로킹 `read()`가 `EAGAIN`을 반환하면(=지금 당장은
더 없음) `PTY_COALESCE_WAIT_MS`(원래 값 2ms)만큼 `poll()`로 한 번 더 대기해 "혹시 뒤이어
오는 트리클"을 합친다.

이 대기는 **뒤에 정말 아무것도 안 올 때도 매번 타임아웃까지 그대로 지불된다** — poll(timeout)의
본질상 "더는 안 온다"는 걸 확인하려면 타임아웃 전체를 기다릴 수밖에 없다. 그런데 **키 입력
echo 1개**(사용자가 문자 하나를 타이핑 → 셸/PTY가 그 1바이트를 그대로 되돌려주는 것)가 정확히
이 "고립된 짧은 출력" 패턴이다. 원 커밋(`c4e9ab3`)의 커밋 메시지는 "체감 불가"라고 판단했지만,
그 실측은 64MiB **대량** drain 기준이었고 고립된 단일 echo 케이스는 별도로 재지 않았다.

### 5.2 실측 (수정 전)

`crates/pty` crate에 실제 PTY를 spawn하는 임시 벤치를 추가해 측정했다(GUI 불필요,
`cargo test -p pty --release -- --ignored`):

- **방법**: `/bin/cat`(stdin을 그대로 stdout에 반향 — 셸 프롬프트/readline 지연이 섞이지 않게
  가장 단순한 echo)을 spawn하고, 1바이트를 `write_input()`한 뒤 출력 채널에 그 바이트가
  도착할 때까지의 벽시계 시간을 30회 측정.
- **결과 (수정 전, `PTY_COALESCE_WAIT_MS = 2`)**: `min=2.183ms p50=2.283ms max=2.332ms
  mean=2.283ms` — **30샘플 전부 예외 없이 2ms대**. `PTY_COALESCE_WAIT_MS`(2ms)를 거의 정확히
  그대로 지불하고 있다는 뜻이다.

### 5.3 수정

`libc::poll`의 timeout은 `c_int` 밀리초 정수라 더 잘게(0.x ms) 쪼갤 수 없다. 이 제약 안에서
가장 작은 유의미한 값인 `PTY_COALESCE_WAIT_MS = 1`(원래 2)로 낮췄다. (더 정밀한 마이크로초
단위 대기를 위해 busy-poll이나 kqueue 기반 나노초 타임아웃으로 바꾸는 방안도 검토했으나,
플랫폼별 분기가 늘고 위험도가 커 "최소 변경" 원칙에 맞지 않아 채택하지 않았다 — §8 참고.)

처음에는 read 크기(직전 `read()`가 반환한 바이트 수)로 "진짜 burst인지 고립 echo인지"를
구분해 고립 echo만 대기를 완전히 건너뛰는 방안을 시도했으나(`PTY_COALESCE_TRIGGER_BYTES`),
**실측으로 반증됐다**: `yes 0123456789 | head -c 200000` 같은 실제 대량 출력도 파이프
스케줄링 특성상 개별 `read()`가 매번 24~468바이트의 **작은** 조각으로 온다는 것을 확인했다
(read 크기 히스토그램 실측). 즉 "이번 read 크기"는 burst 여부를 구분하는 신뢰할 수 있는
신호가 아니었다 — 이 방식은 200,000B 대량 출력을 청크 2개(정상)에서 1,925개로 조각내는
회귀를 만들어 **폐기**했다. 최종 수정은 상수 하나(`PTY_COALESCE_WAIT_MS`)만 낮추는 것으로
확정했다.

### 5.4 실측 (수정 후)

같은 벤치를 수정된 코드로 재실행:

- **고립 1바이트 echo**: `min=25.8µs p50=47.3µs max=80.4µs mean=45.1µs`
  (수정 전 대비 **약 48배 감소**, ms→µs 단위로).

  > 위 두 수치(2.283ms vs 47.3µs)는 `PTY_COALESCE_WAIT_MS`만 다르고 나머지 조건이 완전히
  > 같은 A/B다. 다만 최종 수정값은 `WAIT_MS=1`(0이 아니라 1)이므로, 실제 커밋에 반영된
  > 수정 후 수치는 아래 재측정값이 정확하다.

  **`WAIT_MS=1`(최종 수정값)로 재측정**: `min=1.130ms p50=1.193ms max=1.515ms mean=1.218ms` —
  수정 전(2.283ms) 대비 **중앙값 48% 감소** (poll의 ms 정수 제약상 1ms 미만으로는 못 내렸다).
- **대량 출력 코얼레싱 유지 확인**: `yes 0123456789 | head -c 200000`(200,000B)을
  `WAIT_MS=2`(수정 전)로 받으면 청크 2개, `WAIT_MS=1`(수정 후)로 받으면 청크 4개 —
  **사실상 동일한 수준의 코얼레싱**을 유지한다(원 커밋이 막으려던 "read당 1송신" 회귀
  — 청크 수백~수천 개 — 는 재현되지 않았다).

## 6. 추가한 테스트 (`crates/pty/src/lib.rs`)

TDD로 진행했다: 먼저 위 §5.2 실측으로 수정 전 동작이 "고립 echo가 2ms대"임을 확인한 뒤(=RED
근거), 수정하고 같은 절차로 GREEN을 확인했다. 그 실측 벤치를 정식 회귀 테스트 2개로 남겼다
(`cargo test -p pty`에 포함, `--ignored` 아님):

1. `고립된_1바이트_echo는_coalesce_대기_전체를_지불하지_않는다` — `/bin/cat`으로 10회
   왕복을 재고 중앙값이 1.9ms 미만인지 확인한다(옛 동작 2.28ms는 이 상한을 넘어 실패했을
   것 — CI 스케줄링 지터를 흡수하려 널널하게 잡았다).
2. `대량_출력은_coalesce_wait을_1ms로_낮춰도_소수_청크로_뭉친다` — 200,000B 대량 출력을
   받아 청크 수가 50 미만인지 확인한다(실측 4개 대비 널널한 상한 — 코얼레싱이 완전히
   무너지면 수백 개를 훌쩍 넘는다).

두 테스트 모두 실제 PTY를 spawn하는 통합 테스트라 `--release`/`--test-threads=1`로 3회 반복
실행해 안정성을 확인했다(전부 통과, 타이밍 기반 flake 없음).

## 7. 게이트 결과

워크트리 루트에서 포그라운드로 실행:

| 게이트 | 결과 |
|---|---|
| `cargo test -p pty` (debug) | **38 passed, 0 failed, 1 ignored** (무관한 기존 throughput 벤치) |
| `cargo test -p pty --release` ×3 반복 | **매번 38 passed, 0 failed** — 타이밍 플레이크 없음 |
| `cargo test -p runtime` | **274 passed, 0 failed** (변경 없음 — 회귀 없음 재확인) |
| `cargo test -p deppy-sijo` | 메인 스위트 **1558 passed, 0 failed, 8 ignored** + 보조 스위트(logging/scrollback 등) 전부 green |
| `cargo clippy --workspace --all-targets -- -D warnings` | **경고 0개** |
| `cargo run -q -p xtask -- check-boundary` | **OK** — "UI leaf boundary guard passed; zero allowlist capability" |
| 알려진 플레이키 테스트 `부모_에이전트_세션_마커는_pane에_상속되지_않는다` | 별도 3회 재실행 포함 전부 통과 — 이번 변경과 무관 확인 |

## 8. 미검증 항목 (정직하게 남긴다)

- **실제 체감**: 오케스트레이터 지시에 따라 앱을 빌드·실행하지 않았다. `unix_reader_loop`
  왕복 자체는 실제 PTY로 측정했지만(§5), 그 위에 얹히는 egui 프레임/vsync/렌더 구간은
  GUI 없이는 측정 불가능하다 — 아래 "구조적 최소 지연"은 계산값이지 실측이 아니다.
- **PTY_COALESCE_WAIT_MS을 1ms보다 더 낮추는 방안(kqueue 나노초 타임아웃/busy-poll)**:
  이론상 고립 echo 지연을 마이크로초 단위까지 더 줄일 수 있어 보이지만, 플랫폼별 분기와
  검증 비용이 커 "최소 변경" 범위를 넘는다고 판단해 시도하지 않았다 — 추정이며 미검증.
- **여러 hidden/warm 에이전트 세션이 동시에 출력을 쏟아낼 때**의 `logic()` 이벤트 드레인
  루프(§2에서 나열한 warm workspace `events.drain()` 블록) 누적 비용은, 코드상 각 워크스페이스가
  독립적으로 처리되고 개별 이벤트 수가 `DURABLE_DRAIN_BUDGET`(256)로 유계인 것은 확인했지만,
  워크스페이스 수 × 이벤트 폭주가 겹치는 실제 부하 시나리오의 벽시계 비용은 실측하지 않았다
  (`DEPPY_PERF_HARNESS=1` + `DEPPY_FRAME_STATS=1`로 GUI 빌드가 있어야 잴 수 있다).

## 9. 내 소유 영역 밖에서 발견했지만 고치지 않은 것

- **`App::logic()`(다음 프레임) ↔ `ui()`(이번 프레임)의 intent-queue 구조가 만드는 최소
  1프레임 지연.** 키 입력은 `ui()`에서 `WriteInput` intent를 큐에 올리고, 실제
  `runtime.send_command()`(PTY write 트리거)는 **다음** 프레임의 `logic()`에서
  `poll_workspace_protocol_intents()`가 드레인할 때 일어난다(§3에서 확인한 대로
  `flush_command_repaint()`가 그 다음 프레임을 최대한 빨리 예약하긴 하지만, vsync 게이팅 하에서는
  여전히 최소 1프레임 간격만큼의 지연이 구조적으로 남는다). 이 큐잉/드레인의 내부 구현
  (`queue_protocol_intent`/`take_protocol_intent`/`complete_protocol`/
  `drain_workspace_protocol_intents`)은 다른 에이전트(프로토콜 거부 배너) 소유 영역이라
  **읽기만 하고 고치지 않았다.** 이 구조 자체를 바꾸지 않는 한(=leaf가 직접 IO하지 않는다는
  아키텍처 원칙과 충돌), 60Hz 기준 이론상 최대 ~16.6ms(평균 ~8.3ms)의 추가 프레임 지연은
  남는다 — 다만 이건 버그가 아니라 "leaf는 intent만 올리고 IO는 App이 소유한다"는 이 저장소의
  명시된 아키텍처 원칙의 대가다.
- **`RuntimeEventReceiver::drain()`의 무조건 `Vec::with_capacity(256)` 할당**(§0 표 참고,
  `crates/runtime/src/client.rs:131`) — 측정상 call당 ~44.5ns(그중 ~20ns가 이 할당)로 프레임
  예산(16,000,000ns) 대비 6자리 넘게 작다. 내 소유 영역(`crates/runtime`)이라 고칠 수는 있지만,
  체감에 전혀 기여하지 않는 것을 실측으로 확인했으므로 "근거 없는 최적화"를 피하려고 **의도적으로
  수정하지 않았다.**
