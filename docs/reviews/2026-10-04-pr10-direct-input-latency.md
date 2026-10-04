# PR10 — AI 터미널 직접 입력의 프레임 대기와 전송 전 유실 수정

- 기준: `9c7af797a4db4786b0dcc1187980e7ba71ead1de`
- 작업 트리: `/private/tmp/deppy-audit-pr10-wavefinal-followups-20261004`
- 범위: 직접 터미널의 keyboard/IME/paste → Workspace intent → App host → 로컬 Runtime 채널. Composer, cloud/guarded prompt, native 키 매핑, 파일 트리 DND는 변경하지 않았다.
- 앱 실행·재시작·종료, 실제 Grok/provider, 사용자 터미널·클립보드·비밀 데이터 접근은 수행하지 않았다. 실행된 PTY는 임시 디렉터리의 테스트 소유 `/bin/sh`/`cat`뿐이다.

## 실제 원인과 수정

로컬 eframe 0.36.1 `native/epi_integration.rs`의 `run_ui` callback은 각 pass에서 `App::logic`을 먼저, `App::ui`를 나중에 실행한다. 기존 App은 logic에서만 Workspace protocol을 drain했고, 직접 입력은 뒤의 UI에서 intent를 만들었다. 따라서 정상 입력도 다음 logic pass까지 기다렸다. Runtime sender의 worker unpark는 이미 즉시 동작하므로 변경하지 않았다.

별개로, Workspace의 기본 8개 queued/in-flight 슬롯이 가득 찼을 때 직접 입력 producer는 소유한 bytes를 버렸다. Runtime의 `try_send`가 Backpressure를 반환할 때도 command가 소비되어 원래 입력을 복구할 수 없었다. 두 경계 모두 실제 RawInput fixture에서 RED를 확인했다.

App composition root에 전용 final-pass adapter를 추가했다. 모든 UI widget과 `flush_render_side_effects` 뒤, `will_discard == false`일 때만 기존 FIFO의 terminal prefix를 최대 8개 nonblocking channel admission한다. 허용 command는 `WriteInput`, `FocusPane`, `Resize`, `ResizeTracked`, `ResizeSplit`, `Scroll`, `ScrollToBottom`, `ScrollToPrompt`다. unsupported/lifecycle head를 만나면 멈추고 다음 logic repaint를 요청한다. Focus/Resize/control을 건너뛰어 입력만 먼저 보내지 않는다. resize를 입력 앞으로 재정렬하거나 final geometry를 보장한다는 의미는 아니다.

실제 eframe처럼 correction pass에서도 logic을 호출하는 fixture를 사용했다. 공통 App full-drain과 poll은 correction pass에서 새로 stage된 protocol을 소비하지 않으며, final UI tail이 admission한다. egui `RawInput::take`가 raw event를 한 번 소비하는 동작도 확인했다. 기존 explicit retirement/close-before-shutdown drain은 lifecycle 계약을 유지한다.

`InProcessRuntimeClient::send_command_owned`는 공통 validation/canonicalization/byte reservation을 사용한다. 채널/byte budget 오류로 **worker에 들어가지 않은** command의 ownership을 반환한다. Backpressure인 terminal command만 원래 operation/generation과 SessionId 그대로 FIFO 앞에 반납하고 16ms deadline 뒤 다시 시도한다. Disconnected/다른 오류에는 자동 재시도를 하지 않으며, 직접 입력 유실은 기존 알림 경로로 표시한다. API는 오류에 작은 command metadata `Box`를 할당하며 body를 별도로 clone하지 않는다. canonicalization은 처음의 여유 capacity를 축소하면서 재할당할 수 있으므로 전체 경로가 zero-copy라는 주장은 하지 않는다.

## 메모리·재시도 계약

| 경계 | 상한/동작 |
|---|---|
| 일반 protocol | 기존 8개 queued + in-flight 슬롯 유지 |
| 직접 terminal pressure reserve | 같은 deque에 최대 8개 추가; 총 16개 queued + in-flight |
| 직접 WriteInput | command당 1MiB; queued + in-flight의 실제 Vec capacity 합계 8MiB |
| 인접 입력 | 같은 원래 Session/operation/generation만 합침; Focus/Resize/control 경계를 건너지 않음 |
| command가 정확히 1MiB인 경우 | 다음 작은 gesture는 여유 슬롯에 별도 FIFO entry로 보관 |
| body growth | geometric growth, 실제 capacity charge 및 reserve 후 aggregate 재검사 |
| idle/Busy | body/retry plan 재구성 없음; one-shot deadline 재등록 |
| hard limit | 새 gesture 전체 거부 + 기존 유실 알림; prefix 부분 전송 없음 |

egui는 delayed repaint에서 `predicted_dt`를 뺀다. 단순 16ms repaint 요청은 기본 예측 프레임보다 짧아 immediate repaint가 되었다. 실제 RED에서 delay 0µs를 확인한 뒤, 예측 프레임 시간을 요청에 더해 실제 admission deadline은 16ms로 유지했다. GREEN에서 busy-only repaint delay는 15,867µs였다. 이 timer는 대기 command가 없으면 등록하지 않는다.

## 실제 측정

단일 실행의 CPU/fixture 측정이며 native Grok 화면 지연, FPS, RSS, 평균/백분위 수치가 아니다.

| fixture | Before | After |
|---|---|---|
| 실제 logic-before-UI RawInput → host | 첫 pass 0 bytes, 두 번째 6 bytes; 135µs/누적264µs | 첫 pass 6 bytes; 140µs |
| blocked 256 × 4KiB adjacent gesture | 첫 구현의 exact reserve: capacity growth 256회 (RED) | geometric growth 9회, 최종 capacity 1,048,576B |
| busy-only repaint | 0µs (RED) | 15,867µs |
| RawInput Korean IME Commit+Enter → 실제 private cat | — | 같은 pass에 1개 정확한 입력 command; host356µs, viewport echo3,223µs |

처음의 기준 소스 fixture에서도 first pass 0/second pass 6 bytes, 141µs/누적355µs를 기록했다. pass 수 제거가 확인된 개선이며 위 microsecond 값의 차이를 native latency 개선율로 해석하지 않는다.

## 실행 기록

모든 Cargo는 아래 shared gate로 실행했다. 각 로그에는 실제 Cargo argv와 결과가 남아 있다. Cargo 실행 중 Rust source를 수정하지 않았다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '<Cargo argv 배열 JSON>' > /tmp/<아래 로그명> 2>&1
```

| 로그 (/tmp/) | 실제 결과 |
|---|---|
| `deppy-pr10-direct-input-before-20261004.log` | RawInput measurement 1 pass; Workspace capacity/host Busy 유실 2 RED |
| `deppy-pr10-direct-input-multipass-red3-20261004.log` | 첫 pressure 수정 + discarded pass 4 pass; 앞 red/red2는 RuntimeAdmissionError 변환 누락 compile 오류 |
| `deppy-pr10-direct-input-green2-20261004.log` | 10 pass/1 fail: private Korean echo matcher가 wide spacer를 포함하여 timeout; 이후 정확한 body assertion과 non-spacer decoding으로 수정 |
| `deppy-pr10-direct-input-coalescing-red2-20261004.log` | 11 pass/2 RED: growth256회와 정확한1MiB 뒤 작은 key 유실; private 실제 PTY echo pass |
| `deppy-pr10-direct-input-repaint-red2-20261004.log` | 정확한 busy-only fixture 0 pass/1 RED, repaint delay0µs |
| `deppy-pr10-direct-input-final-gate-20261004.log` | App `pr10_`14 pass; Runtime owned sender1, 기존 command queue2 pass; 기존 preparation source test의 prefix extraction 실패로 중단 |
| `deppy-pr10-direct-input-final-continuation-20261004.log` | wrapper/owned callee를 각각 검증하도록 수정한 source test1 + 실제 downstream PTY pressure1 pass; xtask negative mutation이8000을8 prefix로 인식한 검사 결함 발견 |
| `deppy-pr10-direct-input-final-continuation2-20261004.log` | 정확한 bound token 검사 xtask1 + check-boundary pass; Workspace339 pass/1 old-contract assertion fail/1 existing ignored |
| `deppy-pr10-direct-input-final-continuation3-20261004.log` | Workspace340 pass/1 existing ignored; App/Runtime/xtask all-target strict Clippy + fmt check pass |
| `deppy-pr10-direct-input-paste-final-20261004.log` | soft8 pressure fixture를 원래 paste bytes/Session/FIFO 확인으로 강화:1 pass; 마지막 source App all-target strict Clippy + fmt check pass |

초기 보조 compile 실패(`green`, `coalescing-red`)는 App에 없는 tempfile 의존성과 RuntimeEvent Debug 사용 때문이었다. 테스트 소유 std 임시 디렉터리/정적인 event 분기 진단으로 수정했다. 첫 repaint fixture의 TexturesDelta 정리 누락도 test-only 수정 후 정확한 RED를 다시 실행했다. 이 실패들을 product regression pass로 세지 않았다.

핵심 실제 argv는 다음과 같다. 첫 named App/Runtime/queue 그룹의 결과는 final-gate, 나머지 계속된 결과는 표의 continuation 로그에 있다.

```text
cargo test --offline --locked -p deppy-sijo pr10_ -- --nocapture --test-threads=1
cargo test --offline --locked -p runtime pr10_ -- --nocapture --test-threads=1
cargo test --offline --locked -p runtime command_queue -- --test-threads=1
cargo test --offline --locked -p runtime every_runtime_sender_uses_shared_preparation_and_nonclone_byte_reservation -- --test-threads=1
cargo test --offline --locked -p runtime tracked_input_reports_actual_pty_queue_pressure -- --test-threads=1
cargo test --offline --locked -p xtask pr10_ -- --nocapture
cargo run --offline --locked -p xtask -- check-boundary
cargo test --offline --locked -p deppy-sijo ui::workspace::tests -- --test-threads=1
cargo clippy --offline --locked -p deppy-sijo -p runtime -p xtask --all-targets -- -D warnings
cargo fmt --all -- --check
```

xtask는 leaf의 Runtime/OS I/O 금지를 유지하며, dedicated host adapters의 호출 allowlist, blocking/broad effect 금지, final tail 위치와 정확한 8개 bound를 검사한다. guarded sender, broad helper, generic helper 및8000 bound로 바꾸는 negative mutation을 실제 실행했다. 이 검사는 architecture regression guard이며 일반적인 security sandbox가 아니다.

## 남는 보장 범위

직접 `WriteInput`의 host Accepted는 Runtime **채널** admission이며 PTY acceptance ACK가 아니다. 기존 worker의 untracked WriteInput은 downstream PTY refusal을 `PtyInputPressure`로 표시하지만 원래 bytes를 ACK에 묶어 돌려주지 않는다. 실행한 기존 tracked PTY pressure fixture는 channel acceptance 뒤 실제 `QueueFull`이 별도로 발생할 수 있음을 확인한다. 이 PR은 downstream input을 무조건 재전송하지 않으며, 모든 입력이 어떤 압력에서도 절대로 유실되지 않는다는 주장을 하지 않는다. queue-pressure 회복은 알려진 전송 전 거부에 한정한다.

[cmux PR8848](https://github.com/manaflow-ai/cmux/pull/8848)은 Ghostty IOSurface frame identity 순환 및 이미 소유한 일반 입력의 shortcut 처리 단축을 다룬다. 여기서 확인한 Deppy 원인은 egui protocol admission 순서와 소유권 유실이다. [Warp 공식 known issues](https://docs.warp.dev/support-and-community/troubleshooting-and-support/known-issues/)는 shell integration/locale/EDR 등 별도의 원인을 설명한다. 외부 renderer 수정을 그대로 적용할 근거는 발견하지 않았다.

앱 버전/lock/release/handoff는 root가 소유한다. 이 isolated 조사·test build는0.5.5 기준이며 배포하지 않았다. root의 통합 소스 리뷰·전체 gate·0.6.0 packaging/version 검증이 후속 단계다.
