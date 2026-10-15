# 한글 초성·자모 분리 GitHub 비교 연구 — 2026-10-15

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/ui/workspace.rs:8244 | pending focus가 진행 중 preedit에도 request_focus를 다시 호출하는 경로 확인 | egui의 명시적 IME interruption과 winit marked-text 초기화 유발 가능 | fix worker의 실제 프레임 RED/GREEN 결과로 검증 |
| medium | third_party/winit-0.30.13/src/platform_impl/macos/view.rs:441 | 입력 언어 전환 직후 첫 lone-jamo를 재시도하는 보호는 없음 | upstream에서 첫 음절만 자모 분리하는 별도 증상 보고 | 실제 OS trace가 이 signature일 때만 별도 수정 검토 |
| low | — | 사용자 재현 표면·OS 입력기·재현 시점 미확정 | terminal, 하단 composer, PWA 원인 구별 필요 | 사용자 응답과 해당 표면의 원본 이벤트 대조 |

위는 연구 우선순위이며 사용자 장애의 원인 확정 목록이 아니다. 첫 행은 현재 repository source에서 직접 확인한 경로이고, 아직 이 연구 worker가 테스트를 실행한 것은 아니다. 두번째 행은 upstream의 미병합 제안과 증상 대응이며 Deppy의 실제 OS 재현이 아니다.

## 범위와 보존

- 지정 worktree: `/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015`, HEAD `98bc72bfcf1af84eff0996f512bd7495aaf96b8b`. parent가 전달한 product baseline은 main `9ed5fba1`과 동일하다.
- AGENTS.md와 docs/CODEX_HANDOFF.md 상단 읽음. root가 handoff를 수정 중임을 관찰했으며 이 worker는 repository 파일을 변경하지 않았다.
- 이 보고서만 작성했다. Cargo, 제품 앱 stop/launch/restart, 계정·입력 언어·clipboard 조작, 버전 변경, 코드 수정, 테스트 실행을 하지 않았다.
- 근거: 현재 소스·locked egui 0.36.1 소스, GitHub 공식 저장소 issue/PR 원문, GitHub API의 실제 `patch`, 고정 commit 소스. 검색 결과의 요약만으로 구현을 추정하지 않았다.
- upstream contributor의 재현/테스트 주장은 해당 저자의 주장으로만 취급했다. 직접 실행 결과와 합산하지 않는다.

## 현재 세 입력 경로의 계약

### A. macOS native terminal

1. vendored winit는 AppKit `interpretKeyEvents`를 먼저 호출한다. `setMarkedText`는 Preedit를, marked text가 존재할 때 `insertText`는 empty Preedit 후 Commit을 발생시킨다. 키 처리 뒤 IME 개입 여부와 `forward_key_to_app`로 일반 KeyboardInput 발행을 결정한다. [현재 view.rs:269](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/third_party/winit-0.30.13/src/platform_impl/macos/view.rs:269), [insertText:392](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/third_party/winit-0.30.13/src/platform_impl/macos/view.rs:392), [keyDown:441](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/third_party/winit-0.30.13/src/platform_impl/macos/view.rs:441).
2. winit PR4478의 immediate marked-text clear와 후행 ASCII 전달이 이미 backport되어 있다. 한 번 더 가져올 패치가 아니다. [DEPPY_BACKPORT.md](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/third_party/winit-0.30.13/DEPPY_BACKPORT.md).
3. Workspace는 preedit 문자열·active char range·SessionId owner를 보존하고 draw 전 같은 프레임 이벤트를 표시용으로 투영한다. 첫 preedit frame은 공식 IME owner가 아직 없어도 admission한다. TextEdit·popup·modal exclusion이 우선한다. [state:1855](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:1855), [owner gates:7802](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:7802), [admission:10603](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:10603).
4. renderer는 현재/새 preedit 중 자동 `request_focus`를 피한다. preedit 중 focus interruption이 자모를 강제 확정할 수 있다는 기존 주석과 보호가 존재한다. [renderer:590](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/terminal/src/renderer_egui.rs:590).
5. Commit/Text/물리 문장부호는 한 batch의 개수 원장으로 중복 제거한다. Enter가 Commit보다 먼저 또는 다음 프레임에 도착해도 owner별 PendingImeSubmit에 보존하여 완성문자→Enter→후행키 순서를 유지한다. pane 전환의 이전 owner Commit도 별도 처리한다. [reconciliation:10791](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:10791), [PendingImeSubmit:1862](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:1862).

새 로컬 후보: renderer의 보호와 별개로, pending focus 소비 block은 같은 frame에서 `request_terminal_focus`를 호출한다. 해당 helper는 같은 id 여부와 무관하게 `response.request_focus()`를 호출한다. [pending:8230](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:8230), [helper:10343](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/workspace.rs:10343). locked egui memory의 request_focus는 항상 interrupt_ime를 요청하고, egui-winit는 `should_interrupt_composition`을 false/true toggle로 구현한다. vendor의 false는 marked text를 지우고 Disabled로 바꾸며, true는 ime_allowed만 되돌린다. 이는 초기/진행 중 조합을 반복 초기화할 수 있는 실재 source 경로다. fix worker가 `arm_terminal_focus`+첫 Preedit의 real egui frame 회귀를 작성 중이라고 공유했으며 테스트 결과는 parent가 확인해야 한다.

### B. 하단 desktop composer

AppKit→winit→egui-winit까지는 A와 공통이다. 이후 terminal의 RawInput→PTY 원장이 아닌 `egui::TextEdit::multiline`과 `BoundedTextBuffer`를 사용한다. 같은 프레임 Ime 이벤트가 있으면 Enter 전송을 차단한다. 제한 초과 paste/IME replacement는 기존 text/cursor/undo를 복구한다. insert 단위 NFC는 이미 존재한다. [composer:962](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/composer.rs:962), [TextEdit:1053](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/composer.rs:1053), [buffer:41](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/app/src/ui/text_input.rs:41).

따라서 native AppKit 첫-key 문제는 A/B 모두에 영향을 줄 수 있지만, Workspace pending terminal-focus 결함이 composer 안의 지속 타이핑과 같은 원인이라고 단정할 수 없다. composer에서만 재현하면 TextEdit focus/ID 변경, pending_cursor·focus 요청, byte-limit rollback과 실제 callback 순서를 우선 대조해야 한다.

### C. PWA/remote composer

브라우저 실제 textarea 값을 buffer로 보관하고 명시적 send 시 한 번에 전송한다. `input`마다 PTY로 자모를 보내지 않는다. compositionstart/end와 isComposing/keyCode229, compositionSettling(다음 task까지)을 검사한다. composition target에는 session/socket/generation/DOM element가 고정된다. 세션/연결 변경과 명시적 interrupt는 textarea를 교체하여 늦은 composition 이벤트를 이전 owner에 가둔다. send 시 NFC로 정규화한다. [handlers:1584](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/web-remote/assets/app.js:1584), [reset:1511](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/web-remote/assets/app.js:1511), [send:1260](/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015/crates/web-remote/assets/app.js:1260).

listener가 상위 composer에 있어도 capture phase이고 `event.target === composerText`를 검사하므로 실제 textarea에서 발생한 이벤트를 받는 구조다. ghostty-web의 잘못된 focus target 사례를 그대로 적용할 근거는 없다. `beforeinput` linebreak는 buffer 안의 개행이며 submit으로 처리하지 않는 계약도 있다.

## 공개 저장소의 실제 수정 비교

### 1. winit: 자모 분리의 직접 증상과 미병합 cold-start 제안

[issue3095](https://github.com/rust-windowing/winit/issues/3095)는 첫 raw KeyboardInput `ㅇ`, 이어지는 Commit `ㅏ`/`ㄴ` 뒤 정상 음절로, 첫 음절이 분리되는 관찰을 담는다. 후속 contributor는 입력 소스 전환 직후 Apple Korean daemon의 mach timeout과 AppKit의 lone-jamo insertText fallback을 원인으로 제시한다. OS 관측은 저자 주장이고 우리 환경에서 확인하지 않았다.

[PR4693](https://github.com/rust-windowing/winit/pull/4693), head [4a2da8232e0d6211a1322ad9c33d45729bc02bfb](https://github.com/rust-windowing/winit/commit/4a2da8232e0d6211a1322ad9c33d45729bc02bfb)는 **OPEN, 미병합**이다. 실제 diff는 Korean source-switch/초기-view 표시, callback batch 기록, 20ms runloop retry, 최대50회, 후속 key FIFO, weak view timer, blur/window-close 취소를 추가한다. 범위는 compatibility Jamo와 U+1100..11FF까지이며 text에 해당 scalar가 하나라도 있는지 검사한다. PR 설명보다 넓은 gate이므로 그대로 복사하면 안 된다. callback batching·lifecycle·timer·fallback policy까지 큰 변경이며 현재 앱의 source trace가 이 signature임을 먼저 확인해야 한다.

한편 [PR4478](https://github.com/rust-windowing/winit/pull/4478), head [dfb23bff7d1c945a580673a9977f2699e0234d91](https://github.com/rust-windowing/winit/commit/dfb23bff7d1c945a580673a9977f2699e0234d91)는 **OPEN**이고 trailing ASCII/중복 Space를 고치는 다른 패치다. Deppy에는 이미 적용되어 있다.

### 2. cmux: 조합 중 자모 leak는 병합, 첫 key 재시도는 미병합

[PR2529](https://github.com/manaflow-ai/cmux/pull/2529)는 [26ffcf12799f7e394aab018a8d05829588a337f1](https://github.com/manaflow-ai/cmux/commit/26ffcf12799f7e394aab018a8d05829588a337f1)로 **MERGED**. 실제 변경은 `keyEvent.composing`이면 raw event의 fallback text를 Ghostty에 보내지 않도록 막는다. scripted native test는 marked text가 유지될 때 jamo event를 text=nil로 전달하는 것을 검사한다. Deppy vendor의 had_ime_input suppression과 목적은 유사하지만 API와 event graph가 다르므로 Swift 조건을 그대로 가져올 수 없다.

[PR11806](https://github.com/manaflow-ai/cmux/pull/11806), head [2f0b567aa8e3a1dd446f96ab616fcf6ed58946e4](https://github.com/manaflow-ai/cmux/commit/2f0b567aa8e3a1dd446f96ab616fcf6ed58946e4)는 **OPEN**. no-marked-before/after, 정확히 하나의 compatibility-jamo string, Apple Korean source, source-id 안정 조건에서 같은 key를 즉시 한 번 reinterpret한다. 4개 scripted regression은 lonejamo retry, 정상 composition, 숫자, ABC source를 구별한다. 저자는 standalone AppKit 실제 IME 재현을 주장하지만 Xcode full build는 하지 않았다고 명시한다. timeout20ms retry와 즉시1회 retry 중 무엇이 Deppy에 맞는지는 이 근거만으로 고를 수 없다.

### 3. egui: focus는 명시적 composition interruption

[PR7983](https://github.com/emilk/egui/pull/7983), merge [33e89e33bec3e0009096323f0832e73265c29e4c](https://github.com/emilk/egui/commit/33e89e33bec3e0009096323f0832e73265c29e4c)는 ownership을 Memory에 집중시키고 request_focus가 IME를 interrupt하는 계약을 도입했다. 이후 [PR8078](https://github.com/emilk/egui/pull/8078), merge [86a7f47738b70e0b5c39950ca0962ee79e128033](https://github.com/emilk/egui/commit/86a7f47738b70e0b5c39950ca0962ee79e128033)는 integration에 interruption을 위임하고 egui-winit에서 false/true toggle을 구현한다. 둘 다 **MERGED**, locked0.36.1 소스에서도 실제 계약을 확인했다. 최신 owns_ime_events는 has_focus를 반환하므로 초기 PR의 2-frame 소유권 설명을 현재 동작으로 오인하면 안 된다.

단순 request_focus가 안전한 idempotent 호출이라는 전제가 현재 버전에서는 틀리다. 현재 pending-focus 후보는 이 계약을 어기는 local caller 문제로 좁힐 수 있다. TextEdit Enter/newline [issue7876](https://github.com/emilk/egui/issues/7876), Backspace double consumption [issue7908](https://github.com/emilk/egui/issues/7908)는 별도 event leak 유형이며 closed 상태만으로 Deppy의 원인 해결을 주장하지 않는다.

### 4. Ghostty: commit text와 control/navigation key를 분리

[PR12447](https://github.com/ghostty-org/ghostty/pull/12447), merge [df4f981592e9145334ada57271858a98728607eb](https://github.com/ghostty-org/ghostty/commit/df4f981592e9145334ada57271858a98728607eb)는 arrow가 preedit를 확정할 때 확정 텍스트를 text-only event로 먼저 보내고 navigation을 별도 replay한다. plain Left는 AppKit이 이미 cursor를 유지하므로 replay하지 않는다. **MERGED**.

후속 [PR12547](https://github.com/ghostty-org/ghostty/pull/12547), merge [f0bb6ed9eee5271cbbcee7b87d252830b90fb719](https://github.com/ghostty-org/ghostty/commit/f0bb6ed9eee5271cbbcee7b87d252830b90fb719)는 모든 preedit commit text를 별도 전송하고 replay할 key를 제한한다. 기존 arrow 전용 수정이 Ctrl+J 등의 원 control input까지 replay하는 회귀를 만들었기 때문이다. **MERGED**. Deppy에서도 조합 문자·Enter·Backspace·arrow·Ctrl keys를 개별 순서로 검증해야 한다는 근거다. 초성-only를 normalization으로 고칠 근거는 아니다.

첫 음절 실패를 직접 겨냥한 [PR9238](https://github.com/ghostty-org/ghostty/pull/9238), commit [68e0f174912a83e20662b0c607c55e8937ab0922](https://github.com/ghostty-org/ghostty/commit/68e0f174912a83e20662b0c607c55e8937ab0922)는 inputContext.handleEvent를 우선 호출하고 consumed이면 raw fallback text를 억제하는 변경이다. **CLOSED, 미병합**. 채택된 해법으로 제시하지 않는다.

### 5. WezTerm: Shift를 IME에 전달하지 않아 된소리가 분리됨

[issue1504](https://github.com/wezterm/wezterm/issues/1504)와 실제 fix [71dae34b7504c191333f4dac0bb51f793620d077](https://github.com/wezterm/wezterm/commit/71dae34b7504c191333f4dac0bb51f793620d077)는 무modifier뿐 아니라 Shift-only도 IME로 보내도록 수정하고, marked-text 상태를 보관하여 아직 확정되지 않은 문자가 raw key로 새지 않게 한다. 후속 [0d6fbc1aa211fb67004f9a25c9ebce8c6fba97d0](https://github.com/wezterm/wezterm/commit/0d6fbc1aa211fb67004f9a25c9ebce8c6fba97d0)는 조합 표시와 dead-key 처리를 보완한다. 현재 Deppy keyDown은 ime_allowed이면 modifier와 무관하게 interpretKeyEvents를 호출하므로 같은 Shift skip 결함은 보이지 않는다. 검증에는 까/싸/짜와 Shift+Enter를 포함한다.

NFD 표시 문제 [issue1474](https://github.com/wezterm/wezterm/issues/1474)의 실제 commit [c68b55ba4fb94d321744870498e8e595078bcb58](https://github.com/wezterm/wezterm/commit/c68b55ba4fb94d321744870498e8e595078bcb58)는 grapheme 폭 상한을 조정한다. 이는 이미 정상 scalar가 도착한 뒤 표시 폭의 문제다. 초성 raw key가 premature commit되는 문제와 구분한다.

### 6. Alacritty: 기존 Deppy backport의 재현 근거

[issue6942](https://github.com/alacritty/alacritty/issues/6942)는 Korean 조합 다음 첫 숫자/문장부호가 빠지는 경우, [issue8079](https://github.com/alacritty/alacritty/issues/8079)는 한 번 누른 Space가 두 번 입력되는 경우다. GitHub에서 현재 둘 다 open임을 확인했다. 관련 actual winit4478 diff는 immediate marked clear, second insertText 이후 일반 key forwarding, doCommandBySelector의 Commit 상태 early return 제거다. 현재 Deppy는 이 세 가지를 이미 포함한다. 이런 종료키 이슈를 사용자의 초성-only 원인으로 동일시하지 않는다.

### 7. Warp: 실제 공개 소스 범위 확인

과거 Warp가 비공개였다는 일반 기억에 기대지 않았다. 현재 public 공식 repo에는 app와 crates가 있으며, 이전 연구의 고정 commit [5af88f49f84e70025f9c19e13f6b9ae64b624627](https://github.com/warpdotdev/warp/commit/5af88f49f84e70025f9c19e13f6b9ae64b624627)도 GitHub API에서 조회 가능하다. [mac/window.rs](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warpui/src/platform/mac/window.rs#L1573)의 callback은 marked text와 selected range를 dispatch하고 [event.rs](https://github.com/warpdotdev/warp/blob/5af88f49f84e70025f9c19e13f6b9ae64b624627/crates/warpui_core/src/event.rs#L207)는 이를 별도 event로 보관한다. 이 사실만으로 Warp가 첫-key 자모 분리를 해결한다고 말할 수 없다. 현재 Deppy도 range/owner 모델을 이미 갖추고 있어 해당 모델을 신규 해결책으로 제안하지 않는다.

### 8. ghostty-web: 실제 focus 대상과 composition listener의 일치

[commit ebcc6eb59a030f648fb6afc98727c585726f93ae](https://github.com/coder/ghostty-web/commit/ebcc6eb59a030f648fb6afc98727c585726f93ae)의 actual diff는 listener를 textarea에 붙이고 focus도 textarea로 돌린다. 상위 container의 contenteditable 제거와 terminator 대기도 포함한다. 다만 포함 PR [169](https://github.com/coder/ghostty-web/pull/169)은 **OPEN, 미병합**이다. commit 존재와 배포·채택 상태를 구분한다. Deppy PWA는 실제 textarea에서 capture되는 input/composition과 buffered send 구조를 이미 보유하므로 그대로 가져올 해법이 아니다.

## 원인별 검증 하네스와 구별 기준

| 원인 | 관찰해야 할 최소 증거 | 실행 가능한 회귀 시나리오 | 현재 판단 |
| --- | --- | --- | --- |
| event ordering | Native callback→winit event→egui frame→PTY bytes의 순서와 소유 세션 | empty Preedit→Commit, Enter→다음 frame Commit, Commit→후행 punctuation, preedit→다음 preedit, same batch commit+new preedit | 기존 다수 보호 있음; 회귀 유지 |
| IME reset/focus | first preedit frame의 should_interrupt_composition과 set_ime_allowed 호출 | arm_terminal_focus→Preeditㅎ→빈 frame→Preedit하→Preedit한→Commit한, expected no intermediate PTY bytes/no interrupt | 새 local pending block 후보, fix worker가 실제 harness 진행 |
| input-source cold start | source-change 뒤 첫 insertText가 marked 없이 lonejamo인지와 OS timeout | 격리 AppKit view에서 한영 전환 후 0/20/100/300ms 입력, 정상 warm path와 비교 | native OS 미재현; 미병합 패치만 존재 |
| duplicate commit | one physical key의 callback 수와 bytes 개수 | 한+Space1/2회, comma1/2회, 한+Enter, 한+Ctrl+J, arrow | #4478와 local ledger 존재 |
| cursor/focus owner | TextEdit owner, pane/session id, popup 상태 | terminal→composer/search→terminal, pane switch, pending runtime focus, 명시적 클릭과 자동 refocus 각각 | 기존 owner exclusion 보존 필요 |
| browser beforeinput/input | event.target/current textarea identity, isComposing, inputType, DOM value, send count | compositionstartㅎ→input하/한→compositionend→final input 양 순서, keyCode229 Enter, Safari isComposing false terminal Enter, disconnect/session change | PWA 현재 buffering/settling/generation 보호 있음 |
| Unicode normalization/render | committed UTF-8 scalar와 표시·복사 결과 | 완성형한, NFD한, compatibilityㅎㅏㄴ, 옛한글 각각 bytes/preview/copy | NFC는 canonical jamo 조합만 해결; premature compatibility jamo commit의 해결책 아님 |

native regression은 기존 `crates/app/src/ui/workspace/tests/ime_input_harness.rs` 및 Workspace real-frame harness를 재사용한다. 핵심 assertion은 최종 문자열 외에도 **중간 PTY bytes 없음**, **자동 focus 경로가 IME interruption을 내지 않음**, **새 owner로 이전 Commit이 새지 않음**이다. framework 출력은 OS callback 전에 결함을 검출할 수 있어 현재 로컬 후보에 적합하다. macOS 실제 IME 입력을 실행한 증거는 별도로 필요하며 이 worker는 사용자 앱을 조작하지 않았다.

PWA는 기존 `crates/web-remote/tests/fixtures/mobile-usability.js`, `pwa-unified-mobile-ui.js`의 dispatch fixtures를 보완할 수 있다. synthetic CompositionEvent는 실제 IME 성공의 증거가 아니므로 Chromium/Safari 실제 입력기 확인과 구분한다. textarea.value는 조합 중 다시 쓰지 않고 DOM이 유지하도록 하는 계약을 검사하며 session/connection 전환에서의 명시적 element retirement는 보존한다.

## 권고와 남은 일

1. 현재 source에서 관찰 가능한 pending-focus interruption 경로를 먼저 RED/GREEN으로 검증한다. fix worker에게 exact upstream contract와 local chain을 공유했다. 수정은 explicit focus gesture·TextEdit/popup exclusion·pending request 단발 소비를 보존해야 한다.
2. 사용자 재현 표면이 오면 해당 경로에 대응한다. desktop 두 표면 모두에서 입력 언어 전환 직후 첫 음절만 분리되면 winit3095/cold-start family가 추가 후보다. 항상 자모별로 분리되면 지속적인 reset 또는 IME 우회 여부가 더 적합하다. PWA에서만 발생하면 native vendor 수정으로 완료를 주장할 수 없다.
3. upstream 미병합 retry를 자동 backport하지 않는다. immediate retry와20ms×50 timer는 semantics·latency·queued key ownership이 다르며, 애초 현재 defect가 local focus reset이면 범위를 불필요하게 넓힌다.
4. 코드 변경·Cargo 검증·handoff 통합·version/release 판단은 root/fix worker 범위다. 이 worker의 완료 결과는 문헌/소스 비교이며 제품 수정 완료 또는 사용자의 실제 IME 해결을 뜻하지 않는다.

실제 수행한 명령은 `rg`, `sed`, `cat`, `git status/rev-parse`, `gh search/issues/pr/api`, web search다. 테스트 통과 주장은 없다. 연구 중 없는 glob `pwa*`, 잘못된 경로 `crates/app/src/platform`, zsh의 미인용 `?ref=...`, 이 gh 버전이 지원하지 않는 `gh pr diff --stat`는 조회 실패했으며 정해진 경로·인용 API/파일 patch 조회로 교정했다. 제품 실행/빌드 실패가 아니다.

## 구현 worker 변경의 읽기 전용 교차 리뷰 — 소스 검토 완료

- 대상 별도 worktree는 `/Users/jr/Desktop/projects/deppy-sijo-ime-fix-20261015`다. `af0fdad1`은 projected preedit가 nonempty일 때 pending focus를 소비하지 않는 최소 수정과 real-frame 2회귀를 포함한다. 초기 first-preedit 회귀가 root 실행에서 RED였다는 worker 통보를 받았지만, 이 연구 worker는 원 test log를 직접 읽지 않았으므로 독립 실행 증거로 세지 않는다.
- 첫 수정은 no-intermediate-PTY, same-owner/transient-focus, 음절 조합·삭제, idle frame, Commit+다음Preedit, 최종Commit, 후속Enter/Paste를 검사한다. 명시적 pane click/scroll-to-bottom path는 제품 diff에서 바뀌지 않는다. 기존 foreign TextEdit, pane/session replacement, 늦은 Commit의 owner 회귀도 보존된다.
- 리뷰에서 추가 경계 하나를 전달했다. pending reservation이 남은 채 `Preedit한 → Enter+emptyPreedit → idle → Commit한`이 여러 프레임으로 나뉘면 empty projection만으로는 native composition 종료를 판별할 수 없다. pending block이 focus를 재요청할 수 있고, transient owner에서는 그보다 앞선 renderer 자동 focus recovery도 reset할 수 있다.
- test-only `8a90788ee7d186e578e5e420a9eb899d4fd7e651`가 delayed Enter/Commit와 empty-only cancel 후 회복을 추가했다. worker는 root가 2PASS/2FAIL로 경계를 실제 검출했다고 보고했다. 이 연구 worker가 그 raw log를 직접 확인한 것은 아니며 독립 실행으로 세지 않는다. 불필요한 timer/미병합 cold-start 패치를 추가하지 않는 범위로 검토했다.
- 최종 준비 diff는 `awaiting_ime_commit`을 현재 세션의 stored preedit/기존 PendingImeSubmit와 현재 batch의 Commit 유무로 매 frame 계산한다. 비어 있는 preview에도 `PreeditView.awaiting_commit`을 전달하여 renderer 자동 복구와 pending-focus 소비가 같은 근거로 native focus를 유지한다. 추가 timer나 durable wait state가 없다. text paint는 기존 nonempty filter를 유지하므로 취소된 문자열을 계속 그리지 않는다.
- 같은 owner·transient owner, 새 preedit·Commit+다음 preedit·delayed Commit·empty-only cancel 후 다음 calm frame recovery의 분기를 source에서 확인했다. TextEdit/popup 제외와 명시적 click/scroll-to-bottom 경로는 변경되지 않았다. owner가 다른 preedit/PendingImeSubmit로 현재 pane을 막는 조건도 추가하지 않았다. `PreeditView` 두 construction site와 변경된 view method 호출을 함께 대조했다. 추가 blocker는 발견하지 못했다.
- 제품 소스·테스트를 직접 수정하지 않았다. 최종 GREEN·기존 explicit-click/세션 격리 회귀 실행은 root가 수행한다. 이 보고서는 source 교차 검토 승인이지 OS native 타이핑 검증 또는 테스트 통과 주장이 아니다. 후속 실제 실패가 나오면 이 결론의 해당 경계를 재검토해야 한다.
