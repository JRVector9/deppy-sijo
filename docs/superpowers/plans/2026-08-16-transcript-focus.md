# 원문에서 그 턴 선택 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 「원문 보기」를 하면 클릭한 카드의 **그 턴**이 원문에서 선택(강조)되고 그 자리로
스크롤된다. 지금은 항상 최신(맨 아래)에서 시작해 오래된 카드를 눌러도 엉뚱한 데가 보인다.

**Architecture:** 스펙 §6이 계약이다. `AgentWorkTurnRow.source_offset`과
`read_conversation`이 **같은 절대 파일 오프셋 좌표계**를 쓴다는 점을 이용한다 —
`ConversationMessage`에 `offset`을 실어 보내면 뷰어가 그 값으로 범위를 잡는다.

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-history` (브랜치 `feat/work-history-depth`).

---

## 고정 API (세 Task가 이 시그니처에 맞춰 동시에 작업한다)

```rust
// agent_transcript.rs — Task A가 만든다
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub text: String,
    pub at: Option<i64>,
    /// 이 메시지 레코드 줄의 **절대 파일 오프셋**. `AgentWorkTurnRow.source_offset`과
    /// 같은 좌표계다(둘 다 `snapshot_lines`에서 나온다).
    pub offset: u64,
}

// ui/transcript_viewer.rs — Task B가 만든다
impl TranscriptViewerUi {
    pub fn set_conversation(
        &mut self,
        result: Result<TranscriptConversation, TranscriptViewError>,
        focus_offset: Option<u64>,
    );
}

/// 강조할 메시지 인덱스 범위. 찾지 못하면 None.
/// 시작 = offset >= focus_offset인 첫 메시지, 끝 = 그 뒤 첫 User 메시지 직전(없으면 끝).
fn focus_range(messages: &[ConversationMessage], focus_offset: u64)
    -> Option<std::ops::Range<usize>>;
```

---

### Task A: `ConversationMessage`에 오프셋을 싣는다

**Files:** `crates/app/src/agent_transcript.rs`

- [ ] **Step 1: 실패하는 테스트**

```rust
    #[test]
    fn 대화_메시지는_레코드_오프셋을_싣는다() {
        let path = 임시_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"첫"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"답"}]}}"#,
        ]);
        let view = read_conversation(&path, AgentKind::Claude).unwrap();
        assert_eq!(view.messages[0].offset, 0, "첫 줄은 0에서 시작한다");
        assert!(
            view.messages[1].offset > view.messages[0].offset,
            "다음 줄은 뒤에 온다: {:?} vs {:?}",
            view.messages[0].offset,
            view.messages[1].offset
        );
    }
```

헬퍼 이름(`임시_transcript`)은 파일의 기존 것으로 바꿔 쓴다.

- [ ] **Step 2: 실패 확인** — `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript`

- [ ] **Step 3: 구현**

`read_conversation`의 루프가 지금 `for (_, line) in snapshot_lines(snapshot)`으로 **오프셋을
버리고 있다**. `for (offset, line) in ...`으로 받아 `ConversationBuilder::push`에 함께 넘긴다.
`push` 시그니처에 `offset: u64`를 더하고 `ConversationMessage`에 실는다.
claude·codex·kimi 세 갈래 전부 같은 루프를 쓰는지 확인하고, 갈라져 있으면 각각 고친다.

- [ ] **Step 4: 통과 확인** — 위 명령 + `cargo test -p deppy-sijo`

- [ ] **Step 5: 커밋**

```bash
git commit -m "feat(app): 대화 메시지에 레코드 오프셋을 싣는다" -- crates/app/src/agent_transcript.rs
```

---

### Task B: 뷰어가 그 턴을 강조하고 그 자리로 스크롤한다

**Files:** `crates/app/src/ui/transcript_viewer.rs`

- [ ] **Step 1: 실패하는 테스트**

```rust
    #[test]
    fn 초점_범위는_그_턴만_잡는다() {
        let messages = vec![
            메시지(ConversationRole::User, "턴1 지시", 0),
            메시지(ConversationRole::Assistant, "턴1 답", 100),
            메시지(ConversationRole::User, "턴2 지시", 200),
            메시지(ConversationRole::Assistant, "턴2 답", 300),
        ];
        assert_eq!(focus_range(&messages, 0), Some(0..2), "다음 User 직전까지");
        assert_eq!(focus_range(&messages, 200), Some(2..4), "마지막 턴은 끝까지");
    }

    #[test]
    fn 초점_범위는_정확히_일치하지_않아도_다음_메시지를_잡는다() {
        // 턴을 연 줄이 노이즈 규칙으로 걸러졌을 수 있다 — 부등호로 잡는다.
        let messages = vec![
            메시지(ConversationRole::User, "턴1", 0),
            메시지(ConversationRole::User, "턴2", 200),
        ];
        assert_eq!(focus_range(&messages, 150), Some(1..2));
    }

    #[test]
    fn 창_밖의_턴은_초점을_잡지_못한다() {
        let messages = vec![메시지(ConversationRole::User, "최근", 900)];
        assert_eq!(focus_range(&messages, 100), Some(0..1), "뒤쪽은 잡는다");
        assert_eq!(focus_range(&messages, 1_000), None, "그보다 뒤는 없다");
    }

    #[test]
    fn kittest_초점을_못_찾으면_안내를_띄운다() {
        // 조용히 최신을 보여주면 사용자는 그게 그 턴인 줄 안다.
        …  // history.transcript.focus_missing 라벨이 보이는지 확인
    }

    #[test]
    fn kittest_초점_범위는_강조_배경을_받는다() {
        …  // 이 저장소의 기존 kittest 관례를 따른다
    }
```

- [ ] **Step 2: 실패 확인** — `cargo test -p deppy-sijo --bin deppy-sijo ui::transcript_viewer`

- [ ] **Step 3: 구현**

- `set_conversation`에 `focus_offset: Option<u64>` 인자 추가. 저장해 두고 `focus_range`로
  범위를 계산해 필드에 담는다(매 프레임 다시 계산하지 않는다).
- `focus_offset`이 `Some`인데 `focus_range`가 `None`이면 `focus_missing` 플래그를 세운다.
- 렌더:
  - `focus_missing`이면 상단에 `history.transcript.focus_missing` 한 줄(기존
    `truncated` 안내와 같은 자리·같은 모양).
  - 범위 안의 메시지는 선택 배경으로 강조한다. 색은 `designall::tokens`에서 고른다 —
    하드코딩 금지, 라이트/다크 둘 다 성립해야 한다. 역할별 배경 위에 겹치므로 두 색이
    싸우지 않는지 확인한다.
  - 새 대화(세대가 바뀐 프레임)에서 **한 번만** 시작 인덱스로 스크롤한다. `show_rows`는
    행 높이가 균일하므로 `vertical_scroll_offset(start as f32 * (row_height + spacing))`으로
    계산할 수 있다. 이후 프레임에는 사용자의 스크롤을 덮지 않는다.
  - `focus_offset`이 `None`이면 지금처럼 `stick_to_bottom` 동작 그대로.

- [ ] **Step 4: 통과 확인** — 위 명령 + `cargo test -p deppy-sijo`

- [ ] **Step 5: 커밋**

```bash
git commit -m "feat(app): 원문 뷰어가 그 턴을 강조하고 그 자리로 스크롤한다" -- crates/app/src/ui/transcript_viewer.rs
```

---

### Task C: App이 `source_offset`을 뷰어까지 전달 + i18n

**Files:** `crates/app/src/app.rs`, `crates/i18n/locales/*/messages.txt`

- [ ] **Step 1: 실패하는 테스트**

```rust
    #[test]
    fn 원문_보기는_그_턴의_오프셋을_함께_넘긴다() {
        let source = include_str!("app.rs");
        let production = source.split_once("#[cfg(test)]\nmod tests").unwrap().0;
        assert!(
            production.contains("focus_offset: row.source_offset"),
            "카드가 가리키는 턴의 오프셋이 IO 요청에 실려야 한다"
        );
    }
```

- [ ] **Step 2: 실패 확인** — `cargo test -p deppy-sijo --bin deppy-sijo 원문_보기는_그_턴`

- [ ] **Step 3: 구현**

- `AppHostIoAction::Transcript`에 `focus_offset: u64` 추가. `ShowTranscript` 핸들러에서
  `row.source_offset`을 싣는다(행은 이미 찾아 두었다).
- `AppHostIoCompletion::Transcript`에도 그 값을 실어 되돌려 보내, 완료 처리부가
  `set_conversation(result, Some(focus_offset))`으로 넘긴다. **세대 검사는 그대로 유지**한다.
- `AppHostIoFallback::Transcript`(패닉/스폰 실패 경로)도 시그니처를 맞춘다.
- 워크스페이스 전환 시 뷰어를 비우는 기존 처리(`is_empty()` 가드)는 그대로 둔다.

i18n 5로케일에 키 하나 추가:

```
ko-KR:    history.transcript.focus_missing = 이 작업의 대화는 원문 창 밖으로 밀렸습니다 — 최근 대화만 표시합니다
en-US:    history.transcript.focus_missing = This turn is outside the loaded window — showing the most recent conversation
```

ja-JP·zh-Hans·zh-Hant도 같은 키를 각 언어로, 그 파일의 `history.transcript.*` 어투에 맞춰
같은 자리에 넣는다.

- [ ] **Step 4: 게이트**

Run: `cargo test -p i18n 2>&1 | tail -5`
Run: `cargo test -p deppy-sijo 2>&1 | tail -10`
Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 실패 0, 경고 0

- [ ] **Step 5: 커밋**

```bash
git commit -m "feat(app): 원문 보기가 그 턴을 찾아 연다" -- crates/app/src/app.rs crates/i18n/locales
```

---

## 자체 검토 결과

- **스펙 커버리지:** §6-2 → Task A, §6-3·6-4 → Task B, §6-5와 전달 경로 → Task C.
- **타입 일관성:** `ConversationMessage.offset`(A) → `focus_range`(B) → `focus_offset`(C).
  세 Task 모두 위 "고정 API" 블록의 시그니처를 그대로 쓴다.
- **순서 제약:** B·C는 A의 `offset` 필드가 있어야 컴파일된다. A를 먼저 끝내고 B·C를 병렬로.
