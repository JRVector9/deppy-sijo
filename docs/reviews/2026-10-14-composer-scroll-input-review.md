# 스크롤백 입력 수정 후 코드 리뷰

검토 커밋: `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, 기준 `6a06d319`.
canonical: `/Users/jr/Desktop/projects/deppy-sijo-performance`.
상태: 리뷰 완료, 추가 수정 1건. 제품 코드·제품 테스트·앱 실행은 변경하지 않았다.

후속 상태: 사용자 “코드 변경해” 요청에 따라 아래 R1을 수정했다. 이 문서는 검토 커밋의 재현 기록이며, 최신 구현과 검사 결과는 [수정 QA](../qa/2026-10-14-wide-cursor-input.md)를 참고한다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/terminal/src/alacritty_backend.rs:805 | native cursor의 wide 문자 뒷칸 보정 누락 | 한글 안내문을 초안으로 오인해 전송 거절 | 입력용 좌표에도 wide owner 보정 및 회귀 추가 |

## R1. 두 칸 문자의 커서 좌표 보정이 빠짐

**[P2], confidence 10/10. 실제 제품 메서드로 재현.**

기존 `input_guard_snapshot()`은 viewport를 사용했고, Alacritty의 `RenderableCursor::new`는 `WIDE_CHAR_SPACER`에 놓인 커서를 앞의 문자 소유 칸으로 보정한다(`third_party/alacritty_terminal-0.26.0/src/term/mod.rs:2400`). 새 `input_snapshot()`은 grid의 원래 커서를 그대로 복사한다.

Codex 입력줄에 dim 스타일의 `한글 힌트`만 있고 커서가 첫 한글의 뒷칸에 있으면, 기존 viewport 커서는 col2, 새 입력 snapshot 커서는 col3이다. 실제 `prompt_row()`는 앞쪽 안내문 판정에 커서 열을 사용한다(`runtime/src/input_admission.rs:204`). 이 좌표 차이 때문에 기존 `Placeholder`가 `Draft`로 바뀌며 `AgentInputGuard::allows()`가 `visible_input_draft` 경로에서 거절한다(:287). 맨 아래에서도 발생할 수 있는 새 회귀다.

재현 ANSI:

```text
\x1b[2J\x1b[H› \x1b[2m한글 힌트\x1b[0m\x1b[1;4H
```

증거:

- 실제 terminal 라이브러리와 제품의 순수 판정 함수 원문을 사용한 probe: `viewport_col=2 input_col=3 viewport_row=Placeholder input_row=Draft`, expected Placeholder assertion 실패, exit101.
- 실제 `crates/runtime/src/input_admission.rs`를 path module로 로드하고 별도 `/bin/cat` PTY의 Session/StatusDetector와 `AgentInputGuard::allows`를 실행: `ascii_placeholder_allowed=true wide_placeholder_allowed=false`, expected allowed assertion 실패, exit101. 실제 사용자 세션에 입력하지 않았다.
- 첫 full-module probe 컴파일은 서로 다른 Cargo dependency cohort를 선택해 타입 불일치했다. 이를 제품 결함으로 세지 않았다. Session fingerprint의 dependency hash를 맞춘 뒤 컴파일 exit0, 위 행동 실패 exit101을 확인했다. provenance와 첫 실패 로그도 보존했다.

권고: 실제 native cursor를 읽는 현재 방식은 유지하고, 반환할 좌표만 `WIDE_CHAR_SPACER`이면 문자 소유 열로 보정한다. vi 탐색 커서나 scrolled viewport를 다시 사용하지 않는다. dim 한글 안내문/실제 초안, bottom/scrolled 화면을 함께 검증해 기존 준비 상태와 초안 보호를 유지해야 한다.

## 검토 및 검증 범위

7개 변경 파일 전체 diff와 모든 `input_snapshot`/`input_guard_snapshot` 호출부, native grid/RenderableCursor/셀 style 변환, runtime의 실제 PTY 예약 직전 guard, 기본 및 opt-in backend 구현을 대조했다. 기본 Alacritty에서 scroll 위치·render damage 보존과 현재 초안/선택창 보호에 위 사항 외 추가 확정 결함은 찾지 못했다.

실제 재실행한 `CARGO_BUILD_JOBS=2 cargo test --locked -p runtime composer_ -- --nocapture`: **6 통과 / 0 실패**, exit0. 이 검사는 일반 빈 편집기·ASCII hint·긴 입력·현재 초안·스크롤을 검증하지만 위 wide cursor 경우를 포함하지 않는다. 전체 707개를 이번 리뷰에서 다시 실행한 것으로 주장하지 않는다. 이전 full runtime의 macOS 메모리 비교 실패는 기존 QA에 기록되어 있으며 이번 신규 리뷰 결함과 별개다.

실험 Ghostty는 live input snapshot override/history API가 없어 scroll offset이 양수이면 입력 근거가 None이고 guarded 입력이 거절된다. 이는 이전 QA에 이미 명시한 지원 범위 제한이며 새 미확인 결함으로 합산하지 않는다. 실제 Ghostty/외부 provider 계정/사용자 GUI E2E는 실행하지 않았다.

제품 source/test는 검토 커밋과 동일(`git diff --exit-code d8826f0b -- crates`, exit0). 버전 변경·앱 재실행·commit·push 없음. 리뷰 문서 및 handoff만 변경했다.

`review` 스킬은 설치된 필수 `review/checklist.md`와 gstack 도구가 없어 워크플로우를 중단했다. SKILL.md Step2의 “If the file cannot be read, STOP and report the error.”에 따른 중단이며, 사용자의 코드 리뷰 요청은 위 직접 소스 검토와 제품 probe로 완료했다. 스킬 완료나 독립 서브에이전트 리뷰로 주장하지 않는다.

증거: `docs/reviews/evidence/2026-10-14-composer-scroll-input/`. 제품 guard probe는 실제 파일을 참조하며, 로컬 rlib 목록/소스 hash는 `guard-provenance.json`, 각 증거 hash는 `manifest.json`에 있다. 재현 코드는 사용자 prompt/DB/토큰에 접근하지 않는다.
