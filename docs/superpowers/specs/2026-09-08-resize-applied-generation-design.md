# 리사이즈 실제 적용 세대 설계

기준: PR D #165 `1dfef2abca2feb79622f08f8cc8572e9b72b60aa`, protocol v13. 부모가 검토한 `/private/tmp/deppy-resize-applied-generation-design.md`를 최종 D 코드와 대조해 최소 범위로 확정한다. 별도 stacked PR이며 base는 `feat/scrollback-live-policy`다.

## 문제와 선택

현재 App의 bounded command queue 수락을 WorkspaceUi가 적용 완료처럼 처리한다. 250ms fence 시계도 여기서 시작하며 target cols/rows만 비교해서 A→B→A의 옛 A와 마지막 A를 구분하지 못한다. 타이머 연장만으로 해결하면 PTY 실패와 늦은 viewport를 구분할 수 없다. viewport에 token만 넣는 방식은 backend 교체/legacy resize를 식별하지 못한다. 따라서 실제 backend+필수 PTY 성공 결과, worker 단조 epoch, 요청 token을 같이 전달하는 접근을 채택한다.

## 계약

- Requested는 UI가 최신 목표 하나를 보관한 상태, Admitted는 bounded queue 수락, Applied는 backend와 살아 있는 PTY의 실제 resize 성공 및 실제 grid 크기 일치, Presentable은 현재 token/epoch/크기의 viewport가 기존 quiet/deadline을 만족한 상태다.
- 기존 120ms debounce, split 최종 크기 예외, stable snapshot, 32ms quiet/250ms hard deadline은 유지한다. hard deadline은 Applied 증거부터 시작한다. 큐 수락이나 이전 요청의 결과로 현재 목표를 승격하지 않는다.
- 요청 token은 UI 수명 nonce와 checked generation이다. worker는 세션별 마지막 요청/결과 하나만 보관하며 같은 token/같은 크기 재전송은 resize/SIGWINCH를 반복하지 않고 ACK한다. 같은 token 다른 크기나 이전 generation은 거부한다.
- worker lifetime에서 단조 epoch를 발급한다. backend 실패/PTY 실패/legacy resize/backend 교체는 이전 성공 token을 무효화한다. archive/inflate와 respawn에서 같은 SessionId라도 epoch를 재사용하지 않는다.
- 첫 tracked 요청 전의 기존 로컬 세션은 legacy Viewport를 유지한다. tracked 이후의 모든 viewport는 epoch/token/실제 크기를 보존한다. UI는 현재 token과 epoch watermark로 늦은 프레임을 fence 종료 뒤에도 거부한다.
- viewport 자체의 stamp도 Applied의 증거다. durable ACK와 최신 viewport slot의 전달 순서를 가정하지 않는다. stale frame은 snapshot뿐 아니라 cursor/selection/cache/paste 상태도 갱신하지 않는다.
- queue-full은 기존 6회 유계 backoff를 재사용한다. ACK 유실은 같은 token으로 2초 간격 최대 2회 재확인한다. 실패/만료는 stable 화면을 유지하고 지속 repaint를 중단한다. hidden/exit/runtime 교체/shutdown에서 상태와 타이머를 정리한다.

## 파일 경계

`terminal/backend.rs` 및 Alacritty/Ghostty 구현: 셀 배열을 만들지 않는 실제 grid dimensions 조회. `session/session.rs`: 오류를 삼키지 않는 checked resize 결과와 기존 wrapper 유지. `runtime/resize.rs`: wire token/stamp/typed failure와 유계 worker 상태. command/event/protocol/lib: append-only v14, 공통 viewport accessor. in_process: 실제 적용 후 ACK 및 모든 viewport emit stamp. remote: plain/keyframe/delta 경로 stamp 유지, stamp가 바뀌면 keyframe. app WorkspaceUi: 기존 delivery/fence에 세대 조건만 추가. app replay와 web-remote 두 소비자는 새 variant 최소 adapter만 추가.

D의 scrollback delivery/설정/locale, C reflow, #159 geometry/font/IME 배치, storage, 앱 배포는 수정하지 않는다. Ghostty dimensions는 render state의 실제 cols/rows를 읽으며 요청을 캐시한 self.cols/rows를 증거로 쓰지 않는다.

## wire 호환

v14로 기존 enum 뒤에만 variant를 추가한다. legacy Resize/Viewport와 D 메시지 bytes/discriminant를 그대로 보존한다. v12/v13 peer는 기존 exact-version handshake의 명시적 VersionMismatch다. ACK 없는 legacy Resize로 조용히 강등하여 Applied로 표시하지 않는다. plain와 delta stamp가 같아야 하며 불일치 delta는 bounded keyframe 재요청을 따른다.

## 메모리와 한계

추가 메모리는 세션별 고정 크기 delivery/worker 상태, stable snapshot 하나와 후보 하나 이하다. 전체 history/snapshot 추가 복사 없이 Arc를 쓴다. viewport는 기존 최신값 slot을 사용한다. 모든 카운터는 checked이며 overflow는 실패로 닫는다. TUI redraw 완료 신호, cooperative reflow, 다중 remote writer 소유권, mixed-version 투명 접속은 범위 밖이다.

## 검증

실제 RED 후 GREEN: admission≠Applied; A→B→A 및 승격 후 late B 거부; 같은 token 재시도 멱등/충돌; 실제 backend/PTY 실패; backend 교체 epoch; legacy bytes; plain+delta stamp 및 keyframe 전환; runtime/session 경계; hidden/exit cleanup; 기존 debounce/split/blank deadline. terminal/session/runtime/web-remote/app 관련 focused와 full relevant 검사, strict clippy, fmt, boundary, bounded Codex 코드 리뷰를 수행한다. 앱 재빌드/재실행 및 화면 PASS 주장은 금지한다.

## 실행 중 확정한 계약 보완

- 다른 owner는 정확히 현재 owner_epoch+1에서만 CAS 전환한다. 같은 owner/epoch는 generation으로 비교하고, 이전 owner epoch는 실제 backend 적용 전에 거부한다. O(1) 상태이며 100회 이상 정상 소유자 교체를 허용한다. 랜덤 nonce 대소 비교 및16개 retired owner 제한은 폐기했다.
- Backend/PTY 일부 실패는 같은 token으로 다시 실제 적용한다. 성공한 token만 멱등 ACK를 재발행한다. UI 자동 재확인은2초 간격 최대2회이고 실패/소진 뒤 안정 화면을 남긴 채 fence/요청을 종료한다.
- 큐 coalesce는 payload token과rollback token을 함께 갱신한다. 큐완료에서는수락만기록하고Applied와동일stamp viewport를받은뒤최종표식을해제한다. 선택해제는실제화면승격의기존공통경로를따른다.
- 구형 v12/v13 peer는 exact-version handshake에서 명확히 거부한다. legacy Resize 자체의 byte/API는 유지하지만 실제 적용 보장으로 조용히 fallback하지 않는다.
