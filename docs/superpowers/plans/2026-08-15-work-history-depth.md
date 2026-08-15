# 작업 이력 깊이 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 이력 카드가 보여주는 대화를 세 층으로 깊게 만든다 — 요약을 400자·4줄로 늘리고(①),
턴당 최근 메시지 5개를 함께 남기고(③), 카드에서 **원문 전체**를 열 수 있게 한다(②).

**Architecture:** 스펙(`docs/superpowers/specs/2026-08-15-work-history-depth-design.md`)이 계약이다.
①은 추출기 상수와 정규화 규칙, ③은 additive 마이그레이션 컬럼 하나(`messages_json`),
②는 저장 없이 **볼 때만** transcript 파일을 읽는 새 뷰어다. 이력 탭 본문은 git 패널과 같은
마스터-디테일(좌 목록 / 우 원문)이 된다.

**Tech Stack:** Rust 2024, egui/eframe 0.35, kittest, rusqlite(forward-only `MIGRATIONS`).

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-history` (브랜치 `feat/work-history-depth`).
메인 워크트리 `/Users/jr/Desktop/projects/deppy-sijo`와 `/Users/jr/Desktop/projects/deppy-git-panel`은
**건드리지 않는다**. 앱 실행·`pkill`·`scripts/dev-run.sh`·`cargo build --release`·`git push`·PR 생성은
서브에이전트가 하지 않는다.

---

## 파일 구조

| 파일 | 책임 | 이번 변경 |
| --- | --- | --- |
| `crates/app/src/agent_transcript.rs` | transcript 파싱 | 요약 정규화(①), 턴 메시지 배열(③), `read_conversation`(②) |
| `crates/app/src/agent_detect.rs` | 에이전트·경로 탐지 | transcript 경로 해석기를 `pub(crate)`로 노출(②) |
| `crates/storage/src/db.rs` | 저장·검증 | `messages_json` 마이그레이션·검증·왕복(③) |
| `crates/app/src/app.rs` | 상태 소유·IO·배선 | 프로젝션(③), 원문 IO·본문 분할(②) |
| `crates/app/src/ui/work_history.rs` | 이력 카드(leaf) | 4줄 렌더(①), 역할 라벨(③), 「원문 보기」 액션(②) |
| `crates/app/src/ui/transcript_viewer.rs` | **신규** 원문 뷰어(leaf) | 역할별 렌더·가상화(②) |
| `crates/i18n/locales/*/messages.txt` | 문구 | 키 8개 추가 |

---

# ① 요약 길이와 줄 보존

### Task 1: `clean_agent_summary`를 400자·4줄로

**Files:**
- Modify: `crates/app/src/agent_transcript.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

`agent_transcript.rs`의 `#[cfg(test)] mod tests`에 추가한다.

```rust
    #[test]
    fn 요약은_줄바꿈을_보존한다() {
        let text = "첫 줄\n둘째 줄\n셋째 줄";
        assert_eq!(clean_agent_summary(text).unwrap(), "첫 줄\n둘째 줄\n셋째 줄");
    }

    #[test]
    fn 요약은_줄_안의_연속_공백만_접는다() {
        let text = "앞     뒤\n다음  줄";
        assert_eq!(clean_agent_summary(text).unwrap(), "앞 뒤\n다음 줄");
    }

    #[test]
    fn 요약은_연속_개행을_하나로_접는다() {
        let text = "위\n\n\n아래";
        assert_eq!(clean_agent_summary(text).unwrap(), "위\n아래");
    }

    #[test]
    fn 요약은_네_줄에서_자른다() {
        let text = "1\n2\n3\n4\n5\n6";
        let summary = clean_agent_summary(text).unwrap();
        assert_eq!(summary.lines().count(), AGENT_SUMMARY_LINES);
        assert!(summary.ends_with('…'), "잘렸으면 말줄임을 붙인다: {summary:?}");
    }

    #[test]
    fn 요약은_사백자에서_자른다() {
        let text = "가".repeat(AGENT_SUMMARY_CHARS + 50);
        let summary = clean_agent_summary(&text).unwrap();
        assert_eq!(summary.chars().count(), AGENT_SUMMARY_CHARS + 1, "본문 + 말줄임");
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn 요약은_노이즈_접두를_계속_거부한다() {
        // 이 규칙은 정확도를 올리는 것이라 상한 변경과 무관하게 유지된다.
        for noise in [
            "<system-reminder>x</system-reminder>",
            "<local-command-stdout>x",
            "<command-name>x",
            "<task-notification>x",
        ] {
            assert!(clean_agent_summary(noise).is_none(), "{noise}");
        }
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: FAIL — `AGENT_SUMMARY_LINES` 없음, 줄바꿈이 공백으로 접힘

- [ ] **Step 3: 구현한다**

상수를 바꾼다.

```rust
/// 카드 한 장이 담는 요약 길이. 120자 한 줄이던 것을 2026-08-15에 늘렸다 — orca의
/// preview 상한(220자)보다 크게 잡되, 카드가 세로로 무한정 자라지 않게 줄 수로도 막는다.
const AGENT_SUMMARY_CHARS: usize = 400;
/// 보존하는 최대 줄 수.
const AGENT_SUMMARY_LINES: usize = 4;
/// 최악의 경우(4바이트 문자 400개) + 말줄임 + 줄바꿈 3개.
const AGENT_SUMMARY_BYTES: usize =
    AGENT_SUMMARY_CHARS * 4 + '…'.len_utf8() + (AGENT_SUMMARY_LINES - 1);
```

`clean_agent_summary`의 본문 루프를 바꾼다. 노이즈 거부·`<image …>` 제거는 **그대로 둔다**.

```rust
    let mut summary = String::with_capacity(text.len().min(AGENT_SUMMARY_BYTES));
    let mut summary_chars = 0_usize;
    let mut lines = 1_usize;
    let mut pending_space = false;
    let mut pending_newline = false;
    let mut truncated = false;
    for ch in visible.chars() {
        // 줄바꿈은 보존한다(연속 개행은 하나로). 줄 안의 공백·제어문자만 접는다.
        if ch == '\n' || ch == '\r' {
            pending_newline |= !summary.is_empty();
            pending_space = false;
            continue;
        }
        if ch.is_whitespace() || ch.is_control() {
            pending_space |= !summary.is_empty() && !pending_newline;
            continue;
        }
        if pending_newline {
            if lines == AGENT_SUMMARY_LINES {
                truncated = true;
                break;
            }
            summary.push('\n');
            lines += 1;
            pending_newline = false;
            pending_space = false;
        }
        if pending_space {
            if summary_chars + 2 > AGENT_SUMMARY_CHARS {
                truncated = true;
                break;
            }
            summary.push(' ');
            summary_chars += 1;
            pending_space = false;
        }
        if summary_chars == AGENT_SUMMARY_CHARS {
            truncated = true;
            break;
        }
        if summary.len().checked_add(ch.len_utf8())? > AGENT_SUMMARY_BYTES - '…'.len_utf8() {
            truncated = true;
            break;
        }
        summary.push(ch);
        summary_chars += 1;
    }
```

말줄임 처리(`if truncated { summary.push('…') }`)와 빈 문자열 반환은 그대로다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: PASS. 기존 테스트가 120자를 상수로 고정하고 있으면 새 값으로 갱신하되,
**거부 규칙·`<image>` 제거 테스트는 손대지 않는다**.

- [ ] **Step 5: 게이트와 커밋**

Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -3`

```bash
git add crates/app/src/agent_transcript.rs
git commit -m "feat(app): 이력 요약을 400자·4줄로 늘리고 줄바꿈을 보존한다"
```

---

### Task 2: 카드가 여러 줄을 보여주게

**Files:**
- Modify: `crates/app/src/ui/work_history.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 접힌_카드_요약은_첫_줄만_쓴다() {
        // 목록의 스캔성이 우선 — 접힘 상태에서 카드 높이가 요약 줄 수마다 달라지면
        // 목록이 들쭉날쭉해진다.
        assert_eq!(collapsed_summary_line("첫 줄\n둘째 줄"), "첫 줄");
        assert_eq!(collapsed_summary_line("한 줄뿐"), "한 줄뿐");
        assert_eq!(collapsed_summary_line(""), "");
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::work_history 2>&1 | tail -20`
Expected: FAIL — `collapsed_summary_line` 없음

- [ ] **Step 3: 구현한다**

```rust
/// 접힌 카드에 쓸 한 줄 — 요약이 여러 줄이어도 첫 줄만 보여준다(2026-08-15).
fn collapsed_summary_line(summary: &str) -> &str {
    summary.lines().next().unwrap_or("")
}
```

접힌 카드의 요약 렌더에서 `summary` 대신 `collapsed_summary_line(&summary)`를 넘긴다
(기존 `.truncate()`는 그대로 — 한 줄 안에서 폭이 모자랄 때 여전히 필요하다).
`instruction`도 같은 규칙을 적용한다(제목 줄이 두 줄로 늘어나면 카드가 흔들린다).

펼친 카드의 `expanded_text`는 이미 `Label::wrap()`이라 여러 줄이 그대로 나온다 — **변경 없음**.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::work_history 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/ui/work_history.rs
git commit -m "feat(app): 이력 카드가 접힘엔 한 줄, 펼침엔 전체 줄을 쓴다"
```

---

# ③ 턴당 최근 메시지 배열

### Task 3: `messages_json` 마이그레이션과 저장 검증

**Files:**
- Modify: `crates/storage/src/db.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn work_turn_messages는_왕복한다() {
        let db = 임시_db();                       // 파일의 기존 헬퍼 이름으로 바꿔 쓴다
        let mut row = 기본_work_turn_upsert();     // 기존 픽스처 헬퍼
        row.messages_json = Some(r#"[{"r":"u","t":"물어봤다","at":1}]"#.to_owned());
        db.upsert_agent_work_turns(&[row.clone()]).unwrap();
        let stored = db.agent_work_turns(&row.workspace_id).unwrap();
        assert_eq!(stored[0].messages_json.as_deref(), Some(r#"[{"r":"u","t":"물어봤다","at":1}]"#));
    }

    #[test]
    fn work_turn_messages는_기존_행에서_null이다() {
        // additive 마이그레이션 — 컬럼이 없던 시절 행은 NULL로 읽히고 카드는 기존 경로로 그린다.
        let db = 임시_db();
        let row = 기본_work_turn_upsert();
        db.upsert_agent_work_turns(&[row.clone()]).unwrap();
        assert_eq!(db.agent_work_turns(&row.workspace_id).unwrap()[0].messages_json, None);
    }

    #[test]
    fn work_turn_messages_상한을_넘으면_거부한다() {
        let mut row = 기본_work_turn_upsert();
        row.messages_json = Some("x".repeat(AGENT_WORK_TURN_MESSAGES_BYTES_MAX + 1));
        assert!(!agent_work_turn_upsert_is_valid(&row));
        row.messages_json = Some("no-nul\0".to_owned());
        assert!(!agent_work_turn_upsert_is_valid(&row));
    }
```

검증 함수의 실제 이름은 파일에서 확인해 맞춘다(`agent_work_turn_upsert_is_valid`는 자리표시다).

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p storage 2>&1 | tail -20`
Expected: FAIL — 필드·상수 없음

- [ ] **Step 3: 구현한다**

`MIGRATIONS` 배열 **끝**에 추가한다(forward-only — 중간에 끼워 넣지 마라).

```rust
    // v36: 턴 하나가 남기는 마지막 요약 하나로는 에이전트가 무엇을 했는지 읽히지
    // 않았다. 턴 안 최신 메시지 5개를 유계 JSON으로 함께 보존한다(2026-08-15).
    // 기존 행은 NULL이고, NULL이면 예전대로 instruction+agent_summary만 보여준다.
    "ALTER TABLE agent_work_turns ADD COLUMN messages_json TEXT;",
```

상수와 필드:

```rust
/// 턴 메시지 배열 컬럼 상한. 행 전체 상한(32KB) 안에서 나머지 필드에 자리를 남긴다.
const AGENT_WORK_TURN_MESSAGES_BYTES_MAX: usize = 8 * 1024;
```

`AgentWorkTurnUpsert`와 `AgentWorkTurnRow`에 `pub messages_json: Option<String>`을 더한다.
`Debug` 구현이 텍스트를 가리는 관례를 따르면(기존 `has_summary` 방식) `has_messages`로 같은
처리를 한다 — **원문이 로그로 새지 않게**.

검증(기존 검증 함수 안, `agent_summary`와 같은 자리):
- `None`이면 통과
- `Some`이면 NUL 없음 + `len() <= AGENT_WORK_TURN_MESSAGES_BYTES_MAX`

읽기(`bounded_optional_text`를 쓰는 자리)에 같은 상한으로 컬럼을 추가한다.
INSERT/SELECT 문 두 곳(upsert의 컬럼 목록·`excluded.messages_json`, 조회 SELECT)에 넣는다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p storage 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/storage/src/db.rs
git commit -m "feat(storage): 작업 이력 턴에 유계 메시지 배열 컬럼을 더한다"
```

---

### Task 4: 턴 수집이 최근 메시지 5개를 남기게

**Files:**
- Modify: `crates/app/src/agent_transcript.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 턴은_최근_메시지_다섯_개를_남긴다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        for index in 0..8 {
            pending.push_message(TurnRole::Assistant, format!("응답 {index}"), Some(index));
        }
        let turn = pending.finish();
        assert_eq!(turn.messages.len(), TURN_MESSAGES_MAX);
        assert_eq!(turn.messages.last().unwrap().text, "응답 7", "최신이 뒤에 온다");
        assert_eq!(turn.messages.first().unwrap().text, "응답 3", "오래된 것이 밀려난다");
    }

    #[test]
    fn 턴_메시지_직렬화는_상한을_넘으면_none이다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        for index in 0..TURN_MESSAGES_MAX {
            pending.push_message(TurnRole::Assistant, "가".repeat(AGENT_SUMMARY_CHARS), Some(index as i64));
        }
        // 5 × 400자 한글(3바이트)이면 6KB 남짓 — 상한 안이라 Some이어야 한다.
        assert!(pending.finish().messages_json().is_some());
    }

    #[test]
    fn 턴_메시지_json은_역할을_한_글자로_쓴다() {
        let mut pending = PendingTurn::new("claude", 0, "지시".to_owned(), None, None);
        pending.push_message(TurnRole::User, "물음".to_owned(), Some(1));
        pending.push_message(TurnRole::Assistant, "답".to_owned(), Some(2));
        let json = pending.finish().messages_json().unwrap();
        assert!(json.contains(r#""r":"u""#), "{json}");
        assert!(json.contains(r#""r":"a""#), "{json}");
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 구현한다**

```rust
/// 턴 하나가 보존하는 메시지 수 — orca의 SESSION_PREVIEW_MESSAGE_LIMIT과 같은 값.
pub const TURN_MESSAGES_MAX: usize = 5;
/// 직렬화 결과 상한 — storage의 컬럼 상한(8KB)과 같은 값이다. 넘으면 None으로 떨어뜨려
/// 저장을 거부당하는 대신 조용히 기존 두 필드로 물러난다(fail-soft).
const TURN_MESSAGES_JSON_BYTES_MAX: usize = 8 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TurnRole {
    User,
    Assistant,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TurnMessage {
    pub role: TurnRole,
    pub text: String,
    pub at: Option<i64>,
}
```

`TurnMessage`의 `Debug`도 텍스트를 `"REDACTED"`로 가린다(이 파일의 기존 관례).

`PendingTurn`과 `TranscriptTurn`에 `messages: Vec<TurnMessage>`를 더하고,

```rust
impl PendingTurn {
    /// 최신 TURN_MESSAGES_MAX개만 남긴다 — 앞에서 밀어낸다.
    fn push_message(&mut self, role: TurnRole, text: String, at: Option<i64>) {
        if text.is_empty() {
            return;
        }
        if self.messages.len() == TURN_MESSAGES_MAX {
            self.messages.remove(0);
        }
        self.messages.push(TurnMessage { role, text, at });
    }
}

impl TranscriptTurn {
    /// storage 컬럼에 넣을 유계 JSON. 상한을 넘으면 None(카드는 기존 두 필드로 그린다).
    pub fn messages_json(&self) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }
        let json = serde_json::to_string(
            &self.messages.iter().map(|m| serde_json::json!({
                "r": match m.role { TurnRole::User => "u", TurnRole::Assistant => "a" },
                "t": m.text,
                "at": m.at,
            })).collect::<Vec<_>>(),
        ).ok()?;
        (json.len() <= TURN_MESSAGES_JSON_BYTES_MAX).then_some(json)
    }
}
```

파서 배선: 지금 `agent_summary`를 세우는 자리에서 `push_message(Assistant, …)`를 함께 부르고,
턴을 여는 사용자 지시에서 `push_message(User, instruction.clone(), occurred_at)`을 부른다.
`clean_agent_summary`를 통과한 텍스트만 넣는다(노이즈는 이미 걸러진다).
claude·codex·kimi 세 파서 모두에 적용한다 — 한 곳만 고치면 다른 에이전트에서 빈 배열이 된다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/agent_transcript.rs
git commit -m "feat(app): 턴이 최근 메시지 5개를 유계로 보존한다"
```

---

### Task 5: 프로젝션이 메시지 배열을 저장하게

**Files:**
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 프로젝션은_턴_메시지를_그대로_올린다() {
        let source = include_str!("app.rs");
        let production = source.split_once("#[cfg(test)]\nmod tests").unwrap().0;
        assert!(
            production.contains("messages_json: turn.messages_json()"),
            "턴 메시지가 upsert에 실려야 한다"
        );
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo 프로젝션은_턴 2>&1 | tail -10`
Expected: FAIL

- [ ] **Step 3: 구현한다**

`app.rs:12507` 근처 `storage::AgentWorkTurnUpsert { … }` 리터럴에 한 줄을 더한다.

```rust
                    messages_json: turn.messages_json(),
```

`work_history_projection_cache`와 `work_history_rows`가 같은 구조체를 쓰면 그쪽 리터럴도
함께 채운다. `WorkHistoryRow`(leaf 뷰 타입)에 `pub messages_json: Option<&'a str>`을 더해
`From<&storage::AgentWorkTurnRow>` 변환에 싣는다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/app.rs
git commit -m "feat(app): 이력 프로젝션이 턴 메시지를 저장한다"
```

---

### Task 6: 펼친 카드가 역할 라벨로 메시지를 보여주게

**Files:**
- Modify: `crates/app/src/ui/work_history.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 카드_메시지는_인접_중복을_지운다() {
        // orca의 dedupeAdjacentConversationTurns와 같은 규칙 — 같은 역할이 같은 말을
        // 연달아 하면 한 번만 보여준다.
        let parsed = parse_turn_messages(
            r#"[{"r":"a","t":"같은 말"},{"r":"a","t":"같은 말"},{"r":"u","t":"다른 말"}]"#,
            "지시",
        );
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn 카드_메시지는_지시와_같은_턴을_지운다() {
        // 제목(instruction)이 본문에 한 번 더 나오는 것을 막는다.
        let parsed = parse_turn_messages(r#"[{"r":"u","t":"지시"},{"r":"a","t":"답"}]"#, "지시");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "답");
    }

    #[test]
    fn 카드_메시지는_손상된_json을_비워서_돌려준다() {
        assert!(parse_turn_messages("{ 망가짐", "지시").is_empty());
        assert!(parse_turn_messages(r#"[{"r":"x","t":"모를 역할"}]"#, "지시").is_empty());
        assert!(parse_turn_messages(&format!("[{}]", r#"{"r":"a","t":"x"},"#.repeat(9)), "지시").is_empty());
    }
```

마지막 단언은 **5개 초과 배열을 통째로 거부**한다는 계약이다(잘라 쓰지 않는다 — 저장 측이
5개를 보장하므로 그보다 많으면 신뢰할 수 없는 입력이다).

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::work_history 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 구현한다**

```rust
/// 카드가 그릴 턴 메시지 하나.
pub struct CardMessage {
    pub role: TurnRole,   // agent_transcript의 것을 재사용한다
    pub text: String,
}

/// `messages_json`을 카드용으로 푼다. 어떤 이유로든 신뢰할 수 없으면 **빈 벡터**를
/// 돌려주고, 호출부는 기존 agent_summary 경로로 물러난다(fail-soft).
fn parse_turn_messages(json: &str, instruction: &str) -> Vec<CardMessage> { … }
```

규칙(테스트가 고정하는 것):
1. 파싱 실패 → 빈 벡터
2. 배열 길이 > `TURN_MESSAGES_MAX` → 빈 벡터
3. `r`이 `"u"`/`"a"`가 아님 → 빈 벡터
4. `instruction`과 정규화 비교(트림 + 연속 공백 접기 + 소문자)해 같은 항목 제거
5. 인접한 같은 역할·같은 정규화 텍스트 제거

펼친 카드에서 「최근 작업」 자리를 이렇게 바꾼다.

```rust
                let messages = row.messages_json.map(|json| parse_turn_messages(json, row.instruction))
                    .unwrap_or_default();
                if messages.is_empty() {
                    // 기존 경로 — agent_summary 한 덩이
                } else {
                    for message in &messages {
                        let label = match message.role {
                            TurnRole::User => catalog.t("history.role.user", &[]),
                            TurnRole::Assistant => catalog.t("history.role.agent", &[]),
                        };
                        expanded_text(ui, &label, &message.text);
                        ui.add_space(4.0);
                    }
                }
```

「요약 복사」 버튼은 그대로 둔다(요약이 있을 때만 나타나는 기존 조건 유지).

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::work_history 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/ui/work_history.rs
git commit -m "feat(app): 펼친 이력 카드가 역할 라벨로 메시지를 보여준다"
```

---

# ② 원문 보기

### Task 7: `read_conversation` 파서

**Files:**
- Modify: `crates/app/src/agent_transcript.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

이 파일의 기존 transcript 픽스처(임시 jsonl 파일을 쓰는 헬퍼)를 찾아 그 방식을 따른다.

```rust
    #[test]
    fn 대화_읽기는_역할_두_개만_남긴다() {
        let path = 임시_transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"물음"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"답"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
        ]);
        let view = read_conversation(&path, AgentKind::Claude).unwrap();
        assert_eq!(view.messages.len(), 2, "tool_use는 제외한다");
        assert_eq!(view.messages[0].role, ConversationRole::User);
        assert_eq!(view.messages[1].text, "답");
    }

    #[test]
    fn 대화_읽기는_메시지당_상한에서_자른다() {
        let long = "가".repeat(CONVERSATION_MESSAGE_BYTES_MAX);   // 3바이트 × N
        let path = 임시_transcript(&[&format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{long}"}}}}"#
        )]);
        let view = read_conversation(&path, AgentKind::Claude).unwrap();
        assert!(view.messages[0].text.len() <= CONVERSATION_MESSAGE_BYTES_MAX);
        assert!(view.messages[0].text.ends_with('…'));
    }

    #[test]
    fn 대화_읽기는_손상된_줄을_건너뛴다() {
        let path = 임시_transcript(&[
            "{ 망가진 줄",
            r#"{"type":"user","message":{"role":"user","content":"살아남는다"}}"#,
        ]);
        let view = read_conversation(&path, AgentKind::Claude).unwrap();
        assert_eq!(view.messages.len(), 1, "한 줄이 깨져도 파일 전체를 버리지 않는다");
    }

    #[test]
    fn 대화_읽기는_메시지_수_상한에서_잘림을_표시한다() {
        let lines: Vec<String> = (0..CONVERSATION_MESSAGES_MAX + 10)
            .map(|index| format!(r#"{{"type":"user","message":{{"role":"user","content":"m{index}"}}}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let view = read_conversation(&임시_transcript(&refs), AgentKind::Claude).unwrap();
        assert_eq!(view.messages.len(), CONVERSATION_MESSAGES_MAX);
        assert!(view.truncated);
        assert_eq!(view.messages.last().unwrap().text, format!("m{}", CONVERSATION_MESSAGES_MAX + 9),
            "최신이 남는다");
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 구현한다**

```rust
/// 원문 보기 전용 상한 — 상태 판정용 tail 파서와 **다른 값**이다. 그쪽은 "지금 무슨
/// 상태인가"를 싸게 알아내는 것이고, 이쪽은 사람이 읽는 것이 목적이라 훨씬 크다.
const CONVERSATION_TAIL_BYTES: u64 = 4 * 1024 * 1024;
pub const CONVERSATION_MESSAGE_BYTES_MAX: usize = 8 * 1024;
pub const CONVERSATION_MESSAGES_MAX: usize = 200;
const CONVERSATION_TOTAL_BYTES_MAX: usize = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConversationRole {
    User,
    Assistant,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub text: String,
    pub at: Option<i64>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct TranscriptConversation {
    pub messages: Vec<ConversationMessage>,
    /// 앞부분이 창 밖으로 밀렸다.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptViewError {
    NotFound,
    ReadFailed,
}

/// **App host 스레드에서만 부른다**(blocking IO).
pub fn read_conversation(path: &Path, kind: crate::agent_detect::AgentKind)
    -> Result<TranscriptConversation, TranscriptViewError>;
```

- `tail_snapshot(path, CONVERSATION_TAIL_BYTES)`로 꼬리를 읽는다(기존 헬퍼 재사용).
  스냅샷이 파일 시작이 아니면 `truncated = true`.
- 줄마다 JSON 파싱 — 실패하면 그 줄만 건너뛴다.
- 역할이 user/assistant인 것만 남긴다. tool_use·thinking·system·meta는 버린다.
- 텍스트는 `clean_agent_summary`의 **노이즈 거부 규칙만** 재사용하고 길이는 자르지 않는다.
  그 뒤 `CONVERSATION_MESSAGE_BYTES_MAX`에서 UTF-8 경계로 자르고 `…`를 붙인다.
  → 거부 규칙을 `fn is_noise_prefix(text: &str) -> bool`로 뽑아 두 곳이 공유하게 한다.
- 누적 바이트가 `CONVERSATION_TOTAL_BYTES_MAX`를 넘거나 메시지가
  `CONVERSATION_MESSAGES_MAX`를 넘으면 **앞에서 버리고** `truncated = true`.
- codex/kimi는 각 파서의 레코드 모양에 맞춰 같은 규칙을 적용한다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_transcript 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/agent_transcript.rs
git commit -m "feat(app): transcript 원문을 유계로 읽는 대화 파서"
```

---

### Task 8: transcript 경로 해석기 노출

**Files:**
- Modify: `crates/app/src/agent_detect.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 세션id로_transcript_경로를_찾는_함수가_공개돼_있다() {
        // 세션이 죽어도 파일은 남는다 — 이력에서 원문을 열려면 이 해석기가 필요하다.
        let missing = transcript_path_for(AgentKind::Claude, "00000000-0000-0000-0000-000000000000", None);
        assert!(missing.is_none(), "없는 세션은 None이다");
        assert!(transcript_path_for(AgentKind::Claude, "../탈출", None).is_none(), "잘못된 id 거부");
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_detect 2>&1 | tail -10`
Expected: FAIL

- [ ] **Step 3: 구현한다**

```rust
/// 저장된 이력 행(kind + 세션ID [+ cwd])에서 transcript 파일을 찾는다. 세션이 이미
/// 끝났어도 파일은 남으므로 원문 보기가 이걸 쓴다(2026-08-15).
pub(crate) fn transcript_path_for(
    kind: AgentKind,
    session_id: &str,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    match kind {
        AgentKind::Claude => find_claude_transcript(session_id),
        // codex 해석기는 cwd 기준이다. 세션ID 기준 경로가 이미 있으면 그것을 먼저 쓰고,
        // 없으면 행의 cwd로 폴백한다.
        AgentKind::Codex => cwd.and_then(|cwd| find_codex_transcript(cwd).map(|(_, path)| path)),
        _ => None,
    }
}
```

구현 전에 `~/.codex/sessions`에서 **세션ID로 직접 찾는 경로가 이미 있는지** 파일을 읽어
확인한다(있으면 그걸 우선 쓰고 cwd 폴백은 뒤에 둔다). `find_claude_transcript`는
`pub(crate)`로 올릴 필요 없이 이 함수 안에서만 쓰면 된다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo agent_detect 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/agent_detect.rs
git commit -m "feat(app): 이력 행에서 transcript 경로를 찾는 해석기"
```

---

### Task 9: 원문 뷰어 모듈

**Files:**
- Create: `crates/app/src/ui/transcript_viewer.rs`
- Modify: `crates/app/src/ui/mod.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 빈_뷰어는_안내_문구를_보여준다() {
        let mut ui = TranscriptViewerUi::default();
        assert!(ui.is_empty());
        ui.set_conversation(Ok(TranscriptConversation::default()));
        assert!(!ui.is_empty(), "열었으면 비어 있지 않다 — 대화가 0건이어도 상태는 열림이다");
    }

    #[test]
    fn kittest_잘린_대화는_상단에_표시를_남긴다() {
        // 렌더 관례는 이 저장소의 기존 kittest 형태를 그대로 복사해 쓴다.
        …
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::transcript_viewer 2>&1 | tail -10`
Expected: FAIL — 모듈 없음

- [ ] **Step 3: 구현한다**

```rust
//! 작업 이력 원문 뷰어 — transcript 파일에서 읽은 대화를 그대로 보여준다.
//! 저장하지 않는다(스펙 §2). leaf는 IO를 하지 않고 App이 읽어 넣어 준다.

#[derive(Default)]
pub struct TranscriptViewerUi {
    conversation: Option<Result<TranscriptConversation, TranscriptViewError>>,
    loading: bool,
}

impl TranscriptViewerUi {
    pub fn set_loading(&mut self);
    pub fn set_conversation(&mut self, result: Result<TranscriptConversation, TranscriptViewError>);
    pub fn is_empty(&self) -> bool;
    pub fn render(&mut self, ui: &mut egui::Ui, catalog: &i18n::Catalog);
}
```

렌더:
- 아무것도 안 열었으면 `history.transcript.empty` 한 줄
- 로딩이면 `history.transcript.loading`
- `NotFound` → `history.transcript.not_found`, `ReadFailed` → `history.transcript.error`
- 잘림이면 맨 위에 `history.transcript.truncated`
- 메시지는 `ScrollArea::vertical().show_rows(...)`로 가상화. 역할 라벨
  (`history.role.user`/`history.role.agent`)과 역할별 배경(사용자는 액센트 계열 옅은 면,
  에이전트는 기본 면 — 이 저장소의 `designall::tokens`에서 고른다)
- 텍스트는 `Label::wrap().selectable(true)`로 선택·복사 가능
- 처음 열 때 스크롤을 **맨 아래**(최신)로

`ui/mod.rs`에 `pub mod transcript_viewer;`를 더한다.

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo --bin deppy-sijo ui::transcript_viewer 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/ui/transcript_viewer.rs crates/app/src/ui/mod.rs
git commit -m "feat(app): 이력 원문 뷰어 렌더 모듈"
```

---

### Task 10: 이력 탭을 마스터-디테일로 + 「원문 보기」 배선

**Files:**
- Modify: `crates/app/src/ui/work_history.rs`
- Modify: `crates/app/src/app.rs`

- [ ] **Step 1: 실패하는 테스트를 쓴다**

```rust
    #[test]
    fn 이력_본문은_목록_360에_원문_나머지다() {
        assert_eq!(history_tab_list_width(1400.0), 360.0);
        assert_eq!(history_tab_list_width(700.0), 280.0, "좁으면 40%");
        assert_eq!(history_tab_list_width(400.0), 220.0, "최소 폭 밑으로는 안 내려간다");
    }

    #[test]
    fn kittest_원문_보기_버튼은_intent를_올린다() {
        // 카드 펼침 토글이 클릭을 삼키지 않아야 한다 — git 패널 행과 같은
        // scope_builder(sense(click)) + widget_info 관례를 쓴다.
        …
        assert!(actions.iter().any(|action| matches!(action, WorkHistoryAction::ShowTranscript(_))));
    }
```

- [ ] **Step 2: 실패를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -20`
Expected: FAIL

- [ ] **Step 3: 구현한다**

leaf(`work_history.rs`):
- `WorkHistoryAction::ShowTranscript(WorkTurnIdentity)` 추가
- 펼친 카드 액션 줄에 「원문 보기」 버튼(`history.action.show_transcript`)

App(`app.rs`):
- 필드 `transcript_viewer_ui: ui::transcript_viewer::TranscriptViewerUi`,
  `transcript_generation: u64`
- `history_tab_list_width(body_width)` — `(body_width * 0.4).clamp(220.0, 360.0)`
- `render_work_history_tab_body`를 좌우 분할로 바꾼다. 좌측은 지금 카드 목록 그대로,
  우측은 `transcript_viewer_ui.render`. 사이 세로 구분선은 git 패널과 같은 관례.
- `ShowTranscript(identity)` 처리: 행에서 `kind`/`agent_session_id`/`cwd`를 꺼내
  `agent_detect::transcript_path_for`로 경로를 구하고, 없으면
  `set_conversation(Err(NotFound))`. 있으면 `set_loading()` 후 off-thread 요청
  (기존 `AppHostIoAction` 경로에 종류 하나 추가, latest-only·in-flight 1개).
  세대 번호로 늦게 온 결과를 버린다(git 패널 IO와 같은 규칙).

- [ ] **Step 4: 통과를 확인한다**

Run: `cargo test -p deppy-sijo 2>&1 | tail -20`
Run: `cargo clippy -p deppy-sijo --all-targets -- -D warnings 2>&1 | tail -3`
Expected: PASS, 경고 0

- [ ] **Step 5: 커밋**

```bash
git add crates/app/src/ui/work_history.rs crates/app/src/app.rs
git commit -m "feat(app): 이력 탭을 목록+원문 마스터-디테일로 만든다"
```

---

### Task 11: i18n 5로케일 + 전체 게이트

**Files:**
- Modify: `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`

- [ ] **Step 1: 코드가 쓰는 키를 전수 조사한다**

```
grep -rno 'catalog\.t("[^"]*"\|text\.t("[^"]*"\|self\.i18n\.t("[^"]*"' crates/app/src --include='*.rs'
```

로 뽑은 집합과 `en-US/messages.txt`를 비교해 **빠진 키를 전부** 넣는다. 이번에 새로 생긴 것:

```
history.action.show_transcript = 원문 보기
history.transcript.empty = 카드에서 「원문 보기」를 누르면 대화가 여기에 표시됩니다
history.transcript.loading = 대화를 읽는 중…
history.transcript.not_found = transcript 파일을 찾지 못했습니다
history.transcript.error = 대화를 읽지 못했습니다
history.transcript.truncated = 앞부분이 잘렸습니다 — 최근 대화만 표시합니다
history.role.user = 나
history.role.agent = 에이전트
```

en-US:

```
history.action.show_transcript = View transcript
history.transcript.empty = Select "View transcript" on a card to read the conversation here
history.transcript.loading = Reading the conversation…
history.transcript.not_found = Transcript file not found
history.transcript.error = Could not read the conversation
history.transcript.truncated = Earlier messages were trimmed — showing the most recent
history.role.user = You
history.role.agent = Agent
```

ja-JP·zh-Hans·zh-Hant도 같은 키를 각 언어로 채운다. 각 파일의 기존 정렬 순서를 따른다.

- [ ] **Step 2: 게이트**

Run: `cargo test -p i18n 2>&1 | tail -5`
Run: `cargo test -p storage 2>&1 | tail -5`
Run: `cargo test -p deppy-sijo 2>&1 | tail -10`
Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 실패 0, 경고 0

- [ ] **Step 3: 커밋**

```bash
git add crates/i18n/locales
git commit -m "feat(i18n): 이력 원문 보기·역할 라벨 문구 (5로케일)"
```

- [ ] **Step 4: 화면 확인 (오케스트레이터가 한다 — 서브에이전트 금지)**

```sh
pkill -x deppy-sijo; (nohup sh scripts/dev-run.sh > /tmp/dr.log 2>&1 &)
```

확인 항목:
1. 이력 카드 요약이 한 줄이 아니라 여러 줄로 보인다(펼쳤을 때)
2. 접힌 목록은 여전히 한 줄씩이라 스캔이 된다
3. 펼친 카드에 「나 / 에이전트」 라벨이 붙은 메시지가 여러 개 보인다
4. 「원문 보기」를 누르면 우측에 대화 전체가 뜬다
5. 오래된(끝난) 세션의 카드에서도 원문이 열린다
6. 대화가 길면 상단에 잘림 표시가 뜨고 스크롤이 맨 아래에서 시작한다
7. codex 세션에서도 1·4가 동작한다(안 되면 경로 해석 한계로 보고)

---

## 자체 검토 결과

- **스펙 커버리지:** §1 → Task 1·2, §2 → Task 7·8·9·10, §3 → Task 3·4·5·6, §4 → Task 11,
  §5 → 각 Task의 테스트 단계.
- **타입 일관성:** `TurnRole`/`TurnMessage`(Task 4) → Task 6 카드 렌더에서 재사용.
  `TranscriptConversation`/`ConversationRole`(Task 7) → Task 9 뷰어, Task 10 배선.
  `messages_json`은 Task 3(저장) → Task 4(생성) → Task 5(전달) → Task 6(표시)로 이어진다.
- **순서 제약:** 4는 3의 컬럼이 있어야 5에서 저장된다. 9는 7의 타입이 필요하다.
  10은 8·9가 모두 끝난 뒤다. 1은 4의 텍스트 규칙을 정하므로 먼저다.
- **병렬 가능:** {1}, {3}, {7}, {8}은 서로 파일이 겹치지 않는다 — 단 1·4·7이 모두
  `agent_transcript.rs`라 **그 셋은 순차**여야 한다.
