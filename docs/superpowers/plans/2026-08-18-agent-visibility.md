# 안 쓰는 에이전트 숨기기 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 런처 카드의 상시 토글로 에이전트를 끄면, 그 카드는 흐려져 고를 수 없게 되고
하단 사용량 바에서도 그 칸이 사라진다.

**Architecture:** 스펙(`docs/superpowers/specs/2026-08-18-agent-visibility-design.md`)이 계약이다.
`AgentsConfig.disabled`(거부 목록)를 App이 소유하고, 런처 leaf는 토글 intent만 올린다.
**탐지는 건드리지 않는다** — 거부 목록은 표시·선택 규칙일 뿐이다.

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-agentvis` (브랜치 `feat/agent-visibility`, `main` 기준).

---

## 고정 API

```rust
// crates/app/src/config.rs — AgentsConfig 안
/// 사용자가 런처에서 끈 에이전트 id("claude"|"codex"|"kimi"). 탐지 결과가 아니라 취향이다.
#[serde(default)]
pub disabled: Vec<String>,

// crates/app/src/agent_launcher.rs (또는 config.rs) — 순수 함수
/// 미지 id 제거 + 중복 제거 + 안정 정렬. 로드·저장 양쪽에서 쓴다.
pub fn normalize_disabled_agents(raw: &[String]) -> Vec<String>;
pub fn agent_is_enabled(disabled: &[String], kind: AgentKind) -> bool;

// crates/app/src/ui/agent_launcher.rs — leaf intent (기존 enum에 추가)
AgentLauncherIntent::SetAgentEnabled { kind: AgentKind, enabled: bool }
```

---

### Task 1: config에 거부 목록

**Files:** `crates/app/src/config.rs` (+ 순수 함수 위치는 구현자가 판단)

- [ ] **Step 1: 실패하는 테스트**

```rust
    #[test]
    fn 거부_목록은_미지_id와_중복을_버린다() {
        let raw = vec!["kimi".into(), "kimi".into(), "없는에이전트".into(), "claude".into()];
        assert_eq!(normalize_disabled_agents(&raw), vec!["claude".to_owned(), "kimi".to_owned()]);
        assert!(normalize_disabled_agents(&[]).is_empty());
    }

    #[test]
    fn 거부_목록에_없으면_켜진_것이다() {
        let disabled = vec!["kimi".to_owned()];
        assert!(!agent_is_enabled(&disabled, AgentKind::Kimi));
        assert!(agent_is_enabled(&disabled, AgentKind::Claude));
        assert!(agent_is_enabled(&[], AgentKind::Kimi), "빈 목록이면 전부 켜짐");
    }
```

- [ ] **Step 2: 실패 확인** — `cargo test -p deppy-sijo --bin deppy-sijo 거부_목록`
- [ ] **Step 3: 구현** — `AgentsConfig.disabled` 추가(`#[serde(default)]`), 두 순수 함수.
      로드 시 정규화하는 자리는 이 저장소의 기존 관례(`shortcuts`가 미지 항목을 버리는 방식)를
      먼저 읽고 같은 자리에 건다.
- [ ] **Step 4: 왕복 테스트** — 저장 → 재로드 후에도 유지되는지(기존 config 왕복 테스트 관례 재사용).
- [ ] **Step 5: 커밋** — `-- crates/app/src/config.rs`

---

### Task 2: 런처 카드 토글

**Files:** `crates/app/src/ui/agent_launcher.rs`

카드 렌더는 `render_agent_list`(약 503행)와 `agent_card`(약 861행)에 있다. 먼저 읽어라.

할 일:
- `show`가 거부 목록(`&[String]`)을 받는다.
- 카드마다 **상시** 토글 스위치. 켜짐/꺼짐 hover 문구는 `launcher.agent.enabled_hint` /
  `launcher.agent.disabled_hint`.
- **꺼진 카드**: 흐리게(muted) 그리고 **선택되지 않는다** — 카드 본문 클릭을 무시한다.
  스위치만 반응한다. 짧은 표시(`launcher.agent.disabled_badge`)를 붙인다.
- 색은 `crate::ui::designall::tokens`에서만 고른다(**하드코딩 금지**, 라이트/다크 둘 다).
- 스위치 클릭 → `AgentLauncherIntent::SetAgentEnabled { kind, enabled }`를 올린다.
  **leaf는 config를 직접 쓰지 않는다** — App이 소유한다(저장소 관례).
- 켜져 있던 현재 선택(`self.selected`)을 끄면 **선택을 비운다**.
- 카드 클릭이 자식 위젯에 삼켜지지 않게 이 저장소의 `scope_builder(UiBuilder::sense(click))`
  + `widget_info` 관례를 확인해 따르되, **스위치는 카드보다 먼저 클릭을 가져가야 한다**
  (egui는 나중에 등록된 위젯이 우선이다 — workspace.rs의 보조 탭 닫기 버튼이 같은 문제를
  같은 방식으로 푼다. 그 코드를 읽고 순서를 맞춰라).

테스트(kittest, 이 파일의 기존 하네스 관례를 복사):
- 꺼진 카드 본문 클릭이 선택을 바꾸지 않는다
- 스위치 클릭이 `SetAgentEnabled`를 올린다
- 현재 선택을 끄면 선택이 비워진다

커밋: `-- crates/app/src/ui/agent_launcher.rs`

---

### Task 3: App 배선 + 사용량 바

**Files:** `crates/app/src/app.rs`

- `SetAgentEnabled` 처리: 거부 목록을 갱신하고 **config를 저장**한다. 저장 경로는 이 저장소의
  기존 설정 저장 방식을 찾아 그대로 쓴다(렌더에서 파일을 쓰지 않는다 — `xtask check-boundary`가
  막는다. 필요하면 기존 `AppControllerAction` 류로 올려라).
- 런처 `show` 호출부에 거부 목록을 넘긴다.
- **사용량 바**: `top_provider_usage`에 넘기기 전에 꺼진 에이전트의 usage를 `None`으로 만든다.
  claude·codex·kimi 셋 다 같은 규칙을 적용한다(지금 요청은 kimi지만 구조를 맞춰 둔다).
  `app.rs` 약 24499행의 `kimi_usage` 조달 지점과 7311행 근처의 폭 계산(`if kimi_usage.is_some()`)을
  함께 확인해라 — 칸이 빠지면 폭도 따라 줄어야 한다.

테스트:
- 순수 함수로 뽑아 값으로 검증: 거부 목록이 주어졌을 때 각 usage가 `None`이 되는지.
- **경계 보장**(스펙): 거부해도 `detect_installed_agents` 결과·이력 행·상태 감지는 그대로다.
  가능한 범위에서 계약 테스트로 고정해라.

커밋: `-- crates/app/src/app.rs`

---

### Task 4: i18n + 전체 게이트

**Files:** `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`

3키: `launcher.agent.enabled_hint`, `launcher.agent.disabled_hint`,
`launcher.agent.disabled_badge`.

ko-KR 초안: `= 이 에이전트를 목록과 사용량에 표시` / `= 꺼짐 — 목록에서 고를 수 없고 사용량도 숨김` / `= 꺼짐`

**주의**: 키를 넣으면 폴백(키 문자열이 라벨로 나옴)에 기대던 kittest가 깨진다. 깨지면 지우거나
약화시키지 말고 `catalog.t(...)` 결과로 겨냥하도록 고쳐라.

게이트:
1. `cargo test -p i18n`
2. `cargo test -p deppy-sijo`
3. `cargo clippy --workspace --all-targets -- -D warnings` 0경고
4. `cargo run -q -p xtask -- check-boundary` 통과

커밋: `-- crates/i18n/locales`

---

## 자체 검토

- 스펙 커버리지: 데이터 모델 → Task 1, 런처 토글·선택 비우기 → Task 2, 사용량 바·저장 → Task 3,
  i18n → Task 4.
- 순서 제약: 1 → 2 → 3 → 4 (2가 1의 순수 함수를, 3이 2의 intent를 쓴다).
- 경계: 숨김은 표시·선택 규칙일 뿐이다. 탐지·이력·상태 감지·이미 뜬 세션은 **건드리지 않는다** —
  이걸 어기면 "숨겼더니 이력이 사라졌다"가 된다.
