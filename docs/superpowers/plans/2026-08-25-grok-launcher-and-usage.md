# Grok Launcher and Remaining Usage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Grok 런처에 실제 모델별 추론 강도 선택을 제공하고 하단 상태바에 주간·월간 잔여율과 크레딧 잔액을 안전하게 표시한다.

**Architecture:** 현재/레거시 Grok 모델 캐시는 `agent_model_catalog`에서 공개 모델 메타데이터만 typed projection으로 변환하고, Grok 설정의 모델·강도 기본값은 감지 snapshot에 한 번만 실어 런처 초기 선택에 사용한다. 사용량은 감지된 Grok 실행 파일을 전용 bounded PTY worker에 전달해 공식 `/usage` 화면을 읽고, 세 숫자만 보존하는 `GrokUsage`를 조건부 네 번째 provider cell로 그린다.

**Tech Stack:** Rust 2024, Serde JSON/TOML, `pty`, `regex`, egui/eframe, egui_kittest, i18n catalog, Cargo/Clippy/rustfmt, macOS codesign

---

설계 기준: `docs/superpowers/specs/2026-08-25-grok-launcher-and-usage-design.md`

## 파일 책임 지도

- `crates/app/src/agent_model_catalog.rs`: Grok 객체/배열 캐시와 `[models]` 기본값을 bounded parse한다.
- `crates/app/src/agent_launcher.rs`: 내장 4.6/4.5 카탈로그, 감지 snapshot의 Grok 기본 강도, 실행 인자 검증을 소유한다.
- `crates/app/src/ui/agent_launcher.rs`: provider 최초 선택과 모델 변경 시 강도 reconcile 규칙을 소유한다.
- `crates/app/src/grok_usage.rs`: 새 파일. `/usage` 화면의 순수 파서, 신선도 상태, 단일 bounded PTY probe를 소유한다.
- `crates/app/src/main.rs`: `grok_usage` 모듈을 등록한다.
- `crates/app/src/app.rs`: 설치 감지 요청, Grok usage snapshot 취득, provider cell rendering을 연결한다.
- `crates/app/src/ui/agent_terminal.rs`: 상태바 인자 전달과 Grok 표시 kittest를 소유한다.
- `crates/i18n/locales/{ko-KR,en-US,ja-JP,zh-Hans,zh-Hant}/messages.txt`: Grok 잔여 사용량의 압축·접근성 문자열을 소유한다.
- `docs/CODEX_HANDOFF.md`: RED/GREEN, 리뷰, 빌드, 남은 작업을 매 단계 기록한다.
- Obsidian `프로젝트 일지/deppy-sijo/2026-08-25 Grok 런처와 잔여 사용량.md`: Workstep 최종 기록이다.

### Task 1: 실제 Grok 캐시 스키마와 설정 기본값을 bounded parse한다

**Files:**
- Modify: `crates/app/src/agent_model_catalog.rs:35-68`
- Modify: `crates/app/src/agent_model_catalog.rs:170-192`
- Modify: `crates/app/src/agent_model_catalog.rs:395-462`
- Modify: `crates/app/src/agent_launcher.rs:755-757`
- Test: `crates/app/src/agent_model_catalog.rs:704-740`
- Test: `crates/app/src/agent_model_catalog.rs:883-927`
- Test: `crates/app/src/agent_model_catalog.rs:1106-1117`
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: 객체형 캐시와 model/effort 설정의 실패 테스트를 추가한다**

배열 fixture는 그대로 두고 다음 객체 fixture와 테스트를 같은 test module에 추가한다.

```rust
const GROK_OBJECT_FIXTURE: &str = r#"{
  "models": {
    "grok-4.6": {
      "api_key": "must-not-be-projected",
      "info": {
        "name": "Grok 4.6",
        "hidden": false,
        "supported_in_api": true,
        "supports_reasoning_effort": true,
        "reasoning_effort": "high",
        "reasoning_efforts": [
          { "value": "xhigh", "default": false },
          { "value": "high", "default": true },
          { "value": "medium", "default": false },
          { "value": "low", "default": false }
        ]
      }
    },
    "grok-4.5": {
      "env_key": "GROK_API_KEY",
      "info": {
        "id": "grok-4.5",
        "name": "Grok 4.5",
        "hidden": false,
        "supported_in_api": true,
        "supports_reasoning_effort": true,
        "reasoning_effort": "high",
        "reasoning_efforts": [
          { "value": "high", "default": true },
          { "value": "medium", "default": false },
          { "value": "low", "default": false }
        ]
      }
    },
    "grok-hidden": {
      "info": { "id": "grok-hidden", "hidden": true, "supported_in_api": true }
    },
    "grok-private": {
      "info": { "id": "grok-private", "hidden": false, "supported_in_api": false }
    },
    "broken": { "info": "not-an-object" }
  }
}"#;

#[test]
fn grok_object_catalog_uses_public_info_key_fallback_and_model_efforts() {
    let models = parse_grok(GROK_OBJECT_FIXTURE, Some("grok-4.6"));
    assert_eq!(values(&models), ["grok-4.6", "grok-4.5"]);
    let grok_46 = models.iter().find(|model| model.value() == "grok-4.6").unwrap();
    assert_eq!(grok_46.label(), "Grok 4.6");
    assert_eq!(
        grok_46.efforts(),
        [
            ReasoningEffort::XHigh,
            ReasoningEffort::High,
            ReasoningEffort::Medium,
            ReasoningEffort::Low,
        ]
    );
    assert_eq!(grok_46.default_effort(), Some(ReasoningEffort::High));
    let grok_45 = models.iter().find(|model| model.value() == "grok-4.5").unwrap();
    assert_eq!(
        grok_45.efforts(),
        [
            ReasoningEffort::High,
            ReasoningEffort::Medium,
            ReasoningEffort::Low,
        ]
    );
    assert!(models.iter().all(|model| !model.label().contains("must-not-be-projected")));
}

#[test]
fn grok_array_catalog_deduplicates_ids_before_the_existing_cap() {
    let models = parse_grok(
        r#"{"models":[
          {"id":"grok-4.5","name":"first"},
          {"id":"grok-4.5","name":"duplicate"},
          {"id":"grok-4.6","name":"second"}
        ]}"#,
        None,
    );
    assert_eq!(values(&models), ["grok-4.5", "grok-4.6"]);
    assert_eq!(models[0].label(), "first");
}

#[test]
fn grok_configured_defaults_read_only_the_nested_models_table() {
    assert_eq!(
        parse_grok_defaults(
            "default = \"wrong\"\ndefault_reasoning_effort = \"xhigh\"\n\
             [models]\ndefault = \"  grok-4.6  \"\n\
             default_reasoning_effort = \" medium \"\n"
        ),
        (
            Some("grok-4.6".to_owned()),
            Some(ReasoningEffort::Medium),
        )
    );
    assert_eq!(
        parse_grok_defaults("[models]\ndefault = \"grok-4.6\"\ndefault_reasoning_effort = \"minimal\"\n"),
        (Some("grok-4.6".to_owned()), None)
    );
}
```

기존 배열 테스트의 호출은 `parse_grok(GROK_FIXTURE, None)`으로 바꾸고 배열 순서·기존
강도 assertion을 그대로 유지한다.

- [ ] **Step 2: 실패가 현재 객체 스키마와 누락 함수 때문인지 확인한다**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_object_catalog_uses_public_info_key_fallback_and_model_efforts --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_configured_defaults_read_only_the_nested_models_table --locked -- --nocapture --test-threads=1
```

Expected: 첫 명령은 새 `parse_grok` 인자/객체 지원 부재로 exit 101, 둘째 명령은
`parse_grok_defaults` 부재로 exit 101이다. 0 tests는 RED 증거로 인정하지 않는다.

- [ ] **Step 3: 객체/배열 projection과 Grok 설정 pair를 최소 구현한다**

`load`가 이미 감지한 기본 모델을 Grok parser에 전달하도록 시그니처를 바꾼다.

```rust
pub(crate) fn load(
    kind: AgentKind,
    home: Option<&Path>,
    configured_default: Option<&str>,
) -> Vec<ModelChoice> {
    let Some(home) = home else {
        return Vec::new();
    };
    match kind {
        AgentKind::Codex => read_bounded(&home.join(".codex/models_cache.json"))
            .map(|text| parse_codex(&text))
            .unwrap_or_default(),
        AgentKind::Kimi => read_bounded(&home.join(".kimi-code/config.toml"))
            .map(|text| parse_kimi(&text))
            .unwrap_or_default(),
        AgentKind::Grok => read_bounded(&home.join(".grok/models_cache.json"))
            .map(|text| parse_grok(&text, configured_default))
            .unwrap_or_default(),
        AgentKind::QwenCode => read_bounded(&home.join(".qwen/settings.json"))
            .map(|text| parse_qwen(&text))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
```

설정 projection은 다음 pair를 사용한다.

```rust
#[derive(Deserialize)]
struct GrokTopLevelConfig {
    #[serde(default)]
    models: Option<GrokModelsSection>,
}

#[derive(Deserialize)]
struct GrokModelsSection {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    default_reasoning_effort: Option<String>,
}

fn parse_grok_defaults(text: &str) -> (Option<String>, Option<ReasoningEffort>) {
    let Some(section) = toml::from_str::<GrokTopLevelConfig>(text)
        .ok()
        .and_then(|config| config.models)
    else {
        return (None, None);
    };
    (
        trimmed_non_empty(section.default),
        trimmed_non_empty(section.default_reasoning_effort)
            .as_deref()
            .and_then(effort_from_value),
    )
}

#[cfg(test)]
fn parse_grok_default_model(text: &str) -> Option<String> {
    parse_grok_defaults(text).0
}

pub(crate) fn grok_configured_defaults(
    home: Option<&Path>,
) -> (Option<String>, Option<ReasoningEffort>) {
    let Some(home) = home else {
        return (None, None);
    };
    let Some(text) = read_bounded(&home.join(".grok/config.toml")) else {
        return (None, None);
    };
    parse_grok_defaults(&text)
}
```

캐시 projection은 cache entry의 `info` 외 sibling을 선언하지 않는다.

```rust
#[derive(Deserialize)]
struct GrokCache {
    models: GrokModelCollection,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GrokModelCollection {
    Array(Vec<serde_json::Value>),
    Object(BTreeMap<String, GrokCacheEntry>),
}

#[derive(Deserialize)]
struct GrokCacheEntry {
    info: serde_json::Value,
}

#[derive(Deserialize)]
struct GrokModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_in_api: Option<bool>,
    #[serde(default)]
    supports_reasoning_effort: bool,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    reasoning_efforts: Vec<GrokReasoningEffort>,
}

fn grok_model_choice(
    value: serde_json::Value,
    fallback_id: Option<String>,
) -> Option<ModelChoice> {
    let model = serde_json::from_value::<GrokModel>(value).ok()?;
    if model.hidden || model.supported_in_api == Some(false) {
        return None;
    }
    let id = trimmed_non_empty(model.id).or_else(|| trimmed_non_empty(fallback_id))?;
    let efforts = if model.supports_reasoning_effort {
        collect_efforts(model.reasoning_efforts.iter().map(|effort| effort.value.as_str()))
    } else {
        Vec::new()
    };
    let default_effort = model
        .reasoning_efforts
        .iter()
        .find(|effort| effort.default)
        .map(|effort| effort.value.as_str())
        .or(model.reasoning_effort.as_deref())
        .and_then(effort_from_value);
    let label = model.name.as_deref().unwrap_or(&id);
    ModelChoice::new(&id, label, efforts, default_effort)
}

fn parse_grok(text: &str, configured_default: Option<&str>) -> Vec<ModelChoice> {
    let Ok(cache) = serde_json::from_str::<GrokCache>(text) else {
        return Vec::new();
    };
    let (entries, object_shaped) = match cache.models {
        GrokModelCollection::Array(values) => (
            values.into_iter().map(|value| (None, value)).collect::<Vec<_>>(),
            false,
        ),
        GrokModelCollection::Object(values) => (
            values
                .into_iter()
                .map(|(id, entry)| (Some(id), entry.info))
                .collect::<Vec<_>>(),
            true,
        ),
    };
    let mut seen = HashSet::new();
    let mut models = entries
        .into_iter()
        .filter_map(|(fallback_id, value)| grok_model_choice(value, fallback_id))
        .filter(|model| seen.insert(model.value().to_owned()))
        .collect::<Vec<_>>();
    if object_shaped
        && let Some(default) = configured_default
        && let Some(position) = models.iter().position(|model| model.value() == default)
    {
        let selected = models.remove(position);
        models.insert(0, selected);
    }
    models.truncate(CATALOG_MODELS_MAX);
    models
}
```

`configured_default_model`의 Grok arm은 `grok_configured_defaults(Some(home)).0`을 사용한다.
같은 Task에서 `agent_launcher::resolve_models`가 새 세 번째 인자를 넘겨 중간 commit도
컴파일 가능하게 유지한다.

```rust
let mut models = if crate::agent_model_catalog::has_disk_catalog(kind) {
    crate::agent_model_catalog::load(kind, home, configured)
} else {
    Vec::new()
};
```

나머지 parser/load test 호출도 새 인자를 명시한다.

```rust
assert!(parse_grok("", None).is_empty());
assert!(parse_grok(r#"{"models": "not-an-array-or-object"}"#, None).is_empty());
assert!(load(AgentKind::Codex, None, None).is_empty());
assert!(load(AgentKind::Claude, Some(Path::new("/")), None).is_empty());
assert!(load(
    AgentKind::Grok,
    Some(Path::new("/deppy-nonexistent-home")),
    None,
)
.is_empty());
```

기존 64개 상한 test의 `parse_grok(&format!(...), None)`과 bounded-read tests의 모든
`load(kind, home, None)`도 같은 규칙으로 바꾼다.

- [ ] **Step 4: catalog 전체 focused tests를 GREEN으로 만든다**

Run:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo agent_model_catalog::tests --locked -- --nocapture --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: `agent_model_catalog::tests`의 모든 선택된 테스트 PASS, 포맷과 diff 검사 exit 0.

- [ ] **Step 5: RED/GREEN 결과를 handoff에 기록하고 커밋한다**

```bash
git add crates/app/src/agent_model_catalog.rs crates/app/src/agent_launcher.rs docs/CODEX_HANDOFF.md
git commit -m "fix(agent): Grok 모델 캐시 스키마 교정"
```

### Task 2: Grok 4.6/4.5와 설정 강도를 런처 초기 선택에 연결한다

**Files:**
- Modify: `crates/app/src/agent_launcher.rs:49-55`
- Modify: `crates/app/src/agent_launcher.rs:406-413`
- Modify: `crates/app/src/agent_launcher.rs:556-664`
- Modify: `crates/app/src/agent_launcher.rs:717-757`
- Modify: `crates/app/src/agent_launcher.rs:1331-1368`
- Modify: `crates/app/src/ui/agent_launcher.rs:757-847`
- Test: `crates/app/src/ui/agent_launcher.rs:1120-1188`
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: 내장 모델·초기 설정·모델 변경의 실패 테스트를 추가한다**

`agent_launcher.rs`에 다음 테스트를 추가한다.

```rust
#[test]
fn grok_builtin_models_match_the_current_cli_effort_ladders() {
    let models = AgentKind::Grok.builtin_model_choices();
    assert_eq!(
        models.iter().map(ModelChoice::value).collect::<Vec<_>>(),
        ["grok-4.6", "grok-4.5"]
    );
    assert_eq!(
        find_model(&models, "grok-4.6").unwrap().efforts(),
        [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ]
    );
    assert_eq!(
        find_model(&models, "grok-4.5").unwrap().efforts(),
        [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ]
    );
}

#[test]
fn grok_46_launch_accepts_xhigh_and_grok_45_rejects_it() {
    let grok = detected(AgentKind::Grok);
    let spec = build_launch_spec(
        &grok,
        LaunchOptions {
            model: "grok-4.6".to_owned(),
            effort: Some(ReasoningEffort::XHigh),
            yolo: false,
        },
        None,
    )
    .unwrap();
    let (_, _, args, _) = spec.into_parts();
    assert_eq!(args, ["--model", "grok-4.6", "--reasoning-effort", "xhigh"]);
    assert!(matches!(
        build_launch_spec(
            &grok,
            LaunchOptions {
                model: "grok-4.5".to_owned(),
                effort: Some(ReasoningEffort::XHigh),
                yolo: false,
            },
            None,
        ),
        Err(LaunchSpecErrorCode::UnsupportedEffort)
    ));
}
```

`ui/agent_launcher.rs`에는 설정 기본값과 사용자 선택 보존을 고정한다.

```rust
#[test]
fn grok_initial_selection_uses_configured_model_and_effort_once() {
    let detected = DetectionSnapshot::from_test_agent_with_defaults(
        AgentKind::Grok,
        PathBuf::from("/tmp/grok"),
        Some("grok-4.6".to_owned()),
        Some(ReasoningEffort::Medium),
    );
    let grok = agent(&detected, AgentKind::Grok);
    let mut ui = AgentLauncherUi::new();
    ui.select(grok);
    assert_eq!(ui.model, "grok-4.6");
    assert_eq!(ui.effort, Some(ReasoningEffort::Medium));

    ui.effort = Some(ReasoningEffort::High);
    ui.select(grok);
    assert_eq!(ui.effort, Some(ReasoningEffort::High));

    ui.model = "grok-4.5".to_owned();
    ui.effort = Some(ReasoningEffort::XHigh);
    ui.reconcile_effort(grok.models());
    assert_eq!(ui.effort, Some(ReasoningEffort::High));
}
```

- [ ] **Step 2: 세 focused RED가 실제로 새 계약 때문에 실패하는지 확인한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_builtin_models_match_the_current_cli_effort_ladders --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_46_launch_accepts_xhigh_and_grok_45_rejects_it --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_initial_selection_uses_configured_model_and_effort_once --locked -- --nocapture --test-threads=1
```

Expected: 내장 4.6 부재, 4.6 unsupported, test snapshot/default effort API 부재 중 해당
원인으로 각각 exit 101.

- [ ] **Step 3: 모델별 내장 사다리와 감지 snapshot 기본 강도를 구현한다**

```rust
const GROK_46_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const GROK_45_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
```

`AgentKind::Grok` 내장 목록은 다음 두 항목으로 바꾼다.

```rust
Self::Grok => &[
    BuiltinModel {
        value: "grok-4.6",
        label: "Grok 4.6",
        efforts: GROK_46_EFFORTS,
        default_effort: Some(ReasoningEffort::High),
    },
    BuiltinModel {
        value: "grok-4.5",
        label: "Grok 4.5",
        efforts: GROK_45_EFFORTS,
        default_effort: Some(ReasoningEffort::High),
    },
],
```

`DetectedAgent`에 `default_effort: Option<ReasoningEffort>`를 추가하고 다음 메서드를
구현한다.

```rust
pub(crate) fn initial_effort(&self, model: &str) -> Option<ReasoningEffort> {
    let choice = find_model(&self.models, model)?;
    self.default_effort
        .filter(|effort| choice.efforts().contains(effort))
        .or_else(|| choice.default_effort())
        .or_else(|| choice.efforts().first().copied())
}
```

테스트 fixture에는 정확한 생성자를 추가한다.

```rust
#[cfg(test)]
pub(crate) fn from_test_agent_with_defaults(
    kind: AgentKind,
    executable: PathBuf,
    default_model: Option<String>,
    default_effort: Option<ReasoningEffort>,
) -> Self {
    Self {
        agents: vec![DetectedAgent {
            kind,
            executable,
            launch_path: None,
            models: kind.builtin_model_choices(),
            default_model,
            default_effort,
        }],
        claude_default_model: None,
        claude_default_effort: None,
    }
}
```

`detect_installed_agents`는 Claude와 Grok 설정을 loop 전에 각각 한 번 읽고 Grok agent에
typed 기본 강도를 싣는다. `resolve_models`는 Task 1의 세 번째 인자를 넘긴다.

```rust
let (grok_default_model, grok_default_effort) =
    crate::agent_model_catalog::grok_configured_defaults(home.as_deref());

let configured = match kind {
    AgentKind::Claude => claude_default_model.clone(),
    AgentKind::Grok => grok_default_model.clone(),
    _ => crate::agent_model_catalog::configured_default_model(kind, home.as_deref()),
};
let default_effort = (kind == AgentKind::Grok).then_some(grok_default_effort).flatten();
DetectedAgent {
    kind,
    executable,
    launch_path: launch_path.clone(),
    models: resolve_models(kind, home.as_deref(), configured.as_deref()),
    default_model: configured,
    default_effort,
}
```

```rust
let mut models = if crate::agent_model_catalog::has_disk_catalog(kind) {
    crate::agent_model_catalog::load(kind, home, configured)
} else {
    Vec::new()
};
```

기존 test constructors는 `default_effort: None`을 명시한다. 알 수 없는 Grok model의
실행 검증용 `fallback_efforts`는 `GROK_45_EFFORTS`, 기본은 `High`를 유지한다.

- [ ] **Step 4: 런처가 설정 강도를 최초/무효화 때만 적용하게 한다**

`reconcile_model`이 교체 여부를 반환하고 `reconcile_options`가 그 경우에만 설정 강도를
사용하게 바꾼다.

```rust
fn select(&mut self, agent: &DetectedAgent) {
    let provider_changed = self.selected != Some(agent.kind());
    if provider_changed {
        self.model.clear();
        self.effort = None;
    }
    self.selected = Some(agent.kind());
    self.error = None;
    self.reconcile_options(agent, provider_changed);
}

fn reconcile_options(&mut self, agent: &DetectedAgent, provider_changed: bool) {
    let model_replaced = self.reconcile_model(agent);
    if provider_changed || model_replaced {
        self.effort = agent.initial_effort(&self.model);
    } else {
        self.reconcile_effort(agent.models());
    }
    if !agent.kind().supports_yolo() {
        self.yolo = false;
    }
}

fn reconcile_model(&mut self, agent: &DetectedAgent) -> bool {
    if crate::agent_launcher::find_model(agent.models(), &self.model).is_some() {
        return false;
    }
    self.model = agent.initial_model().to_owned();
    true
}
```

`reconcile_selection`은 `selected != previous_selected`일 때 `model`과 `effort`를 함께
비우고, `reconcile_options(agent, provider_changed)`를 호출한다. 모델 combo의 직접 변경
경로는 기존 `reconcile_effort(agent.models())`를 유지해 model catalog 기본값을 쓴다.

```rust
let provider_changed = self.selected != previous_selected;
if provider_changed {
    self.model.clear();
    self.effort = None;
}
if let Some(agent) = self.selected.and_then(|kind| snapshot.find(kind)) {
    self.reconcile_options(agent, provider_changed);
}
```

- [ ] **Step 5: launcher/model/UI 회귀를 GREEN으로 만든다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo agent_launcher::tests --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo ui::agent_launcher::tests --locked -- --nocapture --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: 두 test group 전체 PASS, 포맷과 diff 검사 exit 0.

- [ ] **Step 6: handoff를 갱신하고 런처 wave를 커밋한다**

```bash
git add crates/app/src/agent_launcher.rs crates/app/src/ui/agent_launcher.rs docs/CODEX_HANDOFF.md
git commit -m "feat(agent): Grok 모델과 추론 강도 선택 추가"
```

### Task 3: `/usage`의 주간·월간·크레딧을 순수 typed projection으로 파싱한다

**Files:**
- Create: `crates/app/src/grok_usage.rs`
- Modify: `crates/app/src/main.rs:1-35`
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: frozen screen parser RED를 새 모듈에 작성한다**

먼저 `main.rs`에 `mod grok_usage;`를 등록하고 새 파일에 타입과 테스트만 작성한다.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrokCurrency {
    Usd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrokCredits {
    pub(crate) currency: GrokCurrency,
    pub(crate) minor_units: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrokUsage {
    pub(crate) weekly_remaining_percent: Option<u8>,
    pub(crate) monthly_remaining_percent: Option<u8>,
    pub(crate) credits_left: Option<GrokCredits>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_PANEL: &str = "\
Usage
  Context window  41% (205k / 500k)
  WEEKLY
    Weekly limit  30% used  Next reset: 4d 2h
  MONTHLY
    Monthly limit  $15.00 used of $100.00 limit
  Credits left: $12.34
";

    #[test]
    fn full_panel_returns_remaining_windows_and_exact_credits() {
        assert_eq!(
            parse_usage(FULL_PANEL),
            Some(GrokUsage {
                weekly_remaining_percent: Some(70),
                monthly_remaining_percent: Some(85),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 1_234,
                }),
            })
        );
    }

    #[test]
    fn partial_and_redrawn_panels_keep_only_labeled_latest_values() {
        let panel = "\
Weekly limit 90% used
Context window 4% used
Weekly limit\n20% used\nMonthly limit\n15% left\nCredits left: $1,234.50\n";
        assert_eq!(
            parse_usage(panel),
            Some(GrokUsage {
                weekly_remaining_percent: Some(80),
                monthly_remaining_percent: Some(15),
                credits_left: Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: 123_450,
                }),
            })
        );
    }

    #[test]
    fn invalid_or_accountless_panels_do_not_invent_usage() {
        assert_eq!(parse_usage("You are not authenticated"), None);
        assert_eq!(parse_usage("Manage billing to view usage"), None);
        assert_eq!(parse_usage("Context window 41% used"), None);
        assert_eq!(parse_usage("Weekly limit $1 used of $0 limit"), None);
        assert_eq!(parse_usage("Credits left: $18446744073709551616.00"), None);
    }

    #[test]
    fn ansi_is_removed_and_percentages_are_clamped_without_context_false_positives() {
        assert_eq!(
            parse_usage("\u{1b}[31mWeekly limit 999% used\u{1b}[0m"),
            Some(GrokUsage {
                weekly_remaining_percent: Some(0),
                monthly_remaining_percent: None,
                credits_left: None,
            })
        );
    }
}
```

- [ ] **Step 2: 세 parser RED가 누락 함수로 실패하는지 확인한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_usage::tests --locked -- --nocapture --test-threads=1
```

Expected: `parse_usage` 부재로 compile 실패 exit 101. 새 모듈이 0 tests로 빠지면 RED로
인정하지 않는다.

- [ ] **Step 3: terminal 정리, labeled window, fixed-point money parser를 구현한다**

새 파일의 타입 아래에 다음 순수 구현을 추가한다.

```rust
use std::sync::OnceLock;

fn parse_usage(output: &str) -> Option<GrokUsage> {
    let clean = strip_terminal_control_sequences(output);
    let lines = clean.split(['\r', '\n']).collect::<Vec<_>>();
    let usage = GrokUsage {
        weekly_remaining_percent: extract_window_remaining(&lines, "weeklylimit"),
        monthly_remaining_percent: extract_window_remaining(&lines, "monthlylimit"),
        credits_left: extract_credits_left(&lines),
    };
    (usage.weekly_remaining_percent.is_some()
        || usage.monthly_remaining_percent.is_some()
        || usage.credits_left.is_some())
    .then_some(usage)
}

fn extract_window_remaining(lines: &[&str], label: &str) -> Option<u8> {
    static PERCENT: OnceLock<regex::Regex> = OnceLock::new();
    static LIMIT: OnceLock<regex::Regex> = OnceLock::new();
    let percent = PERCENT.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d{1,3})(?:\.\d+)?\s*%\s*(used|left|remaining)")
            .expect("static Grok percent regex")
    });
    let limit = LIMIT.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)\s*(?:used\s*)?of\s*\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)",
        )
        .expect("static Grok limit regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains(label) {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            let compact = compact_label(candidate);
            if candidate != line
                && (compact.contains("weeklylimit") || compact.contains("monthlylimit"))
            {
                break;
            }
            if let Some(captures) = percent.captures(candidate) {
                let value = captures.get(1)?.as_str().parse::<u16>().ok()?.min(100) as u8;
                return match captures.get(2)?.as_str().to_ascii_lowercase().as_str() {
                    "used" => Some(100 - value),
                    "left" | "remaining" => Some(value),
                    _ => None,
                };
            }
            if let Some(captures) = limit.captures(candidate) {
                let used = parse_usd_minor(captures.get(1)?.as_str())?;
                let total = parse_usd_minor(captures.get(2)?.as_str())?;
                return remaining_percent(used, total);
            }
        }
    }
    None
}

fn extract_credits_left(lines: &[&str]) -> Option<GrokCredits> {
    static MONEY: OnceLock<regex::Regex> = OnceLock::new();
    let money = MONEY.get_or_init(|| {
        regex::Regex::new(r"\$\s*([0-9][0-9,]*(?:\.\d{1,2})?)")
            .expect("static Grok money regex")
    });
    for (index, line) in lines.iter().enumerate().rev() {
        if !compact_label(line).contains("creditsleft") {
            continue;
        }
        for candidate in lines.iter().skip(index).take(4) {
            if let Some(captures) = money.captures(candidate) {
                return Some(GrokCredits {
                    currency: GrokCurrency::Usd,
                    minor_units: parse_usd_minor(captures.get(1)?.as_str())?,
                });
            }
        }
    }
    None
}

fn parse_usd_minor(text: &str) -> Option<u64> {
    let normalized = text.replace(',', "");
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(100)?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()?.checked_mul(10)?,
        2 => fraction.parse::<u64>().ok()?,
        _ => return None,
    };
    whole.checked_add(fraction)
}

fn remaining_percent(used: u64, total: u64) -> Option<u8> {
    if total == 0 || used > total {
        return None;
    }
    let remaining = total.checked_sub(used)?;
    let scaled = remaining.checked_mul(100)?;
    let rounded = scaled.checked_add(total / 2)?.checked_div(total)?;
    u8::try_from(rounded.min(100)).ok()
}

fn compact_label(text: &str) -> String {
    text.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn strip_terminal_control_sequences(output: &str) -> String {
    static OSC: OnceLock<regex::Regex> = OnceLock::new();
    static CSI: OnceLock<regex::Regex> = OnceLock::new();
    let osc = OSC.get_or_init(|| {
        regex::Regex::new(r"\x1b\][^\x07]*(?:\x07|\x1b\\)").expect("static OSC regex")
    });
    let csi = CSI
        .get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("static CSI regex"));
    csi.replace_all(&osc.replace_all(output, ""), "").into_owned()
}
```

- [ ] **Step 4: parser 회귀와 정적 검사를 GREEN으로 만든다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_usage::tests --locked -- --nocapture --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: 새 parser tests 전부 PASS, 포맷과 diff 검사 exit 0.

- [ ] **Step 5: parser wave를 기록하고 커밋한다**

```bash
git add crates/app/src/grok_usage.rs crates/app/src/main.rs docs/CODEX_HANDOFF.md
git commit -m "feat(usage): Grok 잔여 사용량 파서 추가"
```

### Task 4: Grok `/usage`를 단일 bounded PTY worker로 조회한다

**Files:**
- Modify: `crates/app/src/grok_usage.rs`
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: 설치 없음·refresh·staleness 상태 전이 RED를 추가한다**

```rust
#[test]
fn probe_admission_requires_an_executable_due_refresh_and_no_pending_job() {
    assert!(!should_start_probe(false, false, None));
    assert!(should_start_probe(true, false, None));
    assert!(!should_start_probe(true, true, None));
    assert!(!should_start_probe(
        true,
        false,
        Some(Duration::from_secs(59))
    ));
    assert!(should_start_probe(
        true,
        false,
        Some(Duration::from_secs(60))
    ));
}

#[test]
fn successful_usage_survives_failures_for_ten_minutes_only() {
    let usage = GrokUsage {
        weekly_remaining_percent: Some(70),
        monthly_remaining_percent: Some(85),
        credits_left: None,
    };
    assert_eq!(fresh_usage_after(usage, Duration::from_secs(599)), Some(usage));
    assert_eq!(fresh_usage_after(usage, Duration::from_secs(600)), Some(usage));
    assert_eq!(fresh_usage_after(usage, Duration::from_secs(601)), None);
}
```

- [ ] **Step 2: focused 상태 RED를 확인한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo probe_admission_requires_an_executable_due_refresh_and_no_pending_job --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo successful_usage_survives_failures_for_ten_minutes_only --locked -- --nocapture --test-threads=1
```

Expected: `should_start_probe`와 `fresh_usage_after` 부재로 각각 exit 101.

- [ ] **Step 3: 단일 pending state와 10분 신선도 경계를 구현한다**

```rust
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use pty::PtyBackend as _;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const STARTUP_DELAY: Duration = Duration::from_secs(2);
const SETTLE_DELAY: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 100_000;

#[derive(Default)]
struct UsageState {
    usage: Option<(Instant, GrokUsage)>,
    pending: Option<mpsc::Receiver<Option<GrokUsage>>>,
    last_request: Option<Instant>,
}

fn should_start_probe(
    executable_present: bool,
    pending: bool,
    since_last_request: Option<Duration>,
) -> bool {
    executable_present
        && !pending
        && since_last_request.is_none_or(|elapsed| elapsed >= REFRESH_INTERVAL)
}

fn fresh_usage_after(usage: GrokUsage, elapsed: Duration) -> Option<GrokUsage> {
    (elapsed <= STALE_AFTER).then_some(usage)
}

pub(crate) fn current(ctx: &egui::Context, executable: Option<&Path>) -> Option<GrokUsage> {
    static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();
    let executable = executable?.to_path_buf();
    let state = STATE.get_or_init(|| Mutex::new(UsageState::default()));
    let Ok(mut state) = state.lock() else {
        return None;
    };
    if let Some(receiver) = state.pending.as_ref() {
        match receiver.try_recv() {
            Ok(Some(usage)) => {
                state.usage = Some((Instant::now(), usage));
                state.pending = None;
            }
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    let since_last_request = state.last_request.map(|requested| requested.elapsed());
    if should_start_probe(true, state.pending.is_some(), since_last_request) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        state.last_request = Some(Instant::now());
        if std::thread::Builder::new()
            .name("grok-usage-probe".to_owned())
            .spawn(move || {
                let usage = fetch_grok_usage(&executable).ok().flatten();
                let _ = sender.send(usage);
                repaint.request_repaint();
            })
            .is_ok()
        {
            state.pending = Some(receiver);
        }
    }
    let (measured_at, usage) = state.usage?;
    fresh_usage_after(usage, measured_at.elapsed())
}
```

- [ ] **Step 4: bounded PTY probe를 구현한다**

```rust
fn usage_panel_rendered(lower: &str) -> bool {
    let compact = compact_label(lower);
    [
        "weeklylimit",
        "monthlylimit",
        "creditsleft",
        "notauthenticated",
        "managebilling",
        "failedtoload",
    ]
    .into_iter()
    .any(|needle| compact.contains(needle))
}

fn fetch_grok_usage(executable: &Path) -> anyhow::Result<Option<GrokUsage>> {
    let program = executable
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Grok executable path is not UTF-8"))?;
    let probe_dir = crate::paths::home_dir()
        .map(|home| home.join(".deppy-sijo").join("usage-probe"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&probe_dir)?;
    let command = pty::CommandSpec {
        program: program.to_owned(),
        args: Vec::new(),
        env: vec![("TERM".to_owned(), "xterm-256color".to_owned())],
        cwd: Some(probe_dir),
    };
    let backend = pty::PortablePtyBackend;
    let mut session = backend.spawn(&command, 120, 40)?;
    let output = session
        .take_output()
        .ok_or_else(|| anyhow::anyhow!("Grok usage PTY output unavailable"))?;
    std::thread::sleep(STARTUP_DELAY);
    session.write_input(b"/usage\r")?;

    let started = Instant::now();
    let mut settle_at = None;
    let mut trusted = false;
    let mut bytes = Vec::new();
    while started.elapsed() < PROBE_TIMEOUT {
        match output.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_OUTPUT_BYTES {
                    bytes.drain(..bytes.len() - MAX_OUTPUT_BYTES);
                }
                let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
                let lower = clean.to_ascii_lowercase();
                if !trusted && lower.contains("trust this folder") {
                    let _ = session.write_input(b"\r");
                    trusted = true;
                    let _ = session.write_input(b"/usage\r");
                }
                if settle_at.is_none() && usage_panel_rendered(&lower) {
                    settle_at = Some(Instant::now() + SETTLE_DELAY);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if settle_at.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
    }
    let _ = session.kill();
    let clean = strip_terminal_control_sequences(&String::from_utf8_lossy(&bytes));
    Ok(parse_usage(&clean))
}
```

실측 테스트는 raw output을 출력하지 않고 명시한 executable에 대해서만 실행한다.

```rust
#[test]
#[ignore = "실제 Grok CLI를 최대 25초 띄운다"]
fn grok_실측_프로브는_민감한_원문_없이_끝난다() {
    let Some(path) = std::env::var_os("DEPPY_GROK_EXECUTABLE").map(std::path::PathBuf::from) else {
        return;
    };
    let usage = fetch_grok_usage(&path).expect("bounded Grok probe");
    assert!(usage.is_none_or(|value| {
        value.weekly_remaining_percent.is_some()
            || value.monthly_remaining_percent.is_some()
            || value.credits_left.is_some()
    }));
}
```

- [ ] **Step 5: worker tests와 bounded 실제 no-auth 동작을 검증한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_usage::tests --locked -- --nocapture --test-threads=1
DEPPY_GROK_EXECUTABLE="$HOME/.nvm/versions/node/v24.18.0/bin/grok" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_실측_프로브는_민감한_원문_없이_끝난다 --locked -- --ignored --nocapture --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: unit tests PASS. ignored probe는 25초 안에 PASS하며 현재 미인증 계정이면 `None`을
정상 결과로 인정한다. raw PTY/account/path output은 테스트와 로그에 나오지 않는다.

- [ ] **Step 6: worker wave를 기록하고 커밋한다**

```bash
git add crates/app/src/grok_usage.rs docs/CODEX_HANDOFF.md
git commit -m "feat(usage): Grok 사용량 프로브 추가"
```

### Task 5: Grok usage를 네 번째 provider cell과 다섯 locale에 연결한다

**Files:**
- Modify: `crates/app/src/app.rs:7323-7540`
- Modify: `crates/app/src/app.rs:12770-12775`
- Modify: `crates/app/src/app.rs:26880-26920`
- Modify: `crates/app/src/app.rs:42486-42555`
- Modify: `crates/app/src/ui/agent_terminal.rs:295-350`
- Modify: `crates/app/src/ui/agent_terminal.rs:1374-1471`
- Modify: `crates/i18n/locales/ko-KR/messages.txt:901-927`
- Modify: `crates/i18n/locales/en-US/messages.txt:901-927`
- Modify: `crates/i18n/locales/ja-JP/messages.txt:901-927`
- Modify: `crates/i18n/locales/zh-Hans/messages.txt:901-927`
- Modify: `crates/i18n/locales/zh-Hant/messages.txt:901-927`
- Modify: `docs/CODEX_HANDOFF.md`

- [ ] **Step 1: 4칸 폭·조건부 Grok cell·localized text의 RED를 작성한다**

`app.rs` 폭 테스트에 다음 assertion을 추가한다.

```rust
assert_eq!(provider_usage_bar_width(4), 810.0);
```

`ui/agent_terminal.rs`의 usage kittest helper에 `grok: Option<GrokUsage>` 인자를 추가하고
다음 세 case를 추가한다.

```rust
let grok = GrokUsage {
    weekly_remaining_percent: Some(70),
    monthly_remaining_percent: Some(85),
    credits_left: Some(crate::grok_usage::GrokCredits {
        currency: crate::grok_usage::GrokCurrency::Usd,
        minor_units: 1_234,
    }),
};
let harness = run(None, some_usage, None, Some(grok), Vec::new());
assert!(harness.query_by_label("Grok logo").is_some());
harness.get_by_label("Grok remaining usage: W 70% · M 85% · $12.34");

let partial = GrokUsage {
    weekly_remaining_percent: Some(70),
    monthly_remaining_percent: None,
    credits_left: None,
};
let harness = run(None, some_usage, None, Some(partial), Vec::new());
harness.get_by_label("Grok remaining usage: W 70%");

let harness = run(None, some_usage, None, None, Vec::new());
assert!(harness.query_by_label("Grok logo").is_none());

let harness = run(None, some_usage, None, Some(grok), vec!["grok".to_owned()]);
assert!(harness.query_by_label("Grok logo").is_none());
```

- [ ] **Step 2: status RED가 새 argument/rendering 부재로 실패하는지 확인한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo 사용량_바_폭은_그려지는_칸_수에_비례한다 --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo kittest_사용량_바_칸은_켜짐_값없음과_꺼짐을_구분해_그린다 --locked -- --nocapture --test-threads=1
```

Expected: 810 assertion 또는 Grok usage argument/cell 부재로 exit 101/테스트 실패.

- [ ] **Step 3: 다섯 locale에 동일한 다섯 키를 추가한다**

각 locale의 `status_bar` 묶음에 다음 exact values를 추가한다.

```text
# ko-KR
status_bar.grok.weekly_short = 주 {value}%
status_bar.grok.monthly_short = 월 {value}%
status_bar.grok.credits_short = {value}
status_bar.grok.accessibility = Grok 잔여 사용량: {values}
status_bar.grok.hover = Grok 주간·월간 잔여율과 크레딧 잔액 — {values}

# en-US
status_bar.grok.weekly_short = W {value}%
status_bar.grok.monthly_short = M {value}%
status_bar.grok.credits_short = {value}
status_bar.grok.accessibility = Grok remaining usage: {values}
status_bar.grok.hover = Grok weekly and monthly remaining usage and credits — {values}

# ja-JP
status_bar.grok.weekly_short = 週 {value}%
status_bar.grok.monthly_short = 月 {value}%
status_bar.grok.credits_short = {value}
status_bar.grok.accessibility = Grok 残り使用量: {values}
status_bar.grok.hover = Grok の週次・月次残り使用量とクレジット — {values}

# zh-Hans
status_bar.grok.weekly_short = 周 {value}%
status_bar.grok.monthly_short = 月 {value}%
status_bar.grok.credits_short = {value}
status_bar.grok.accessibility = Grok 剩余用量：{values}
status_bar.grok.hover = Grok 每周、每月剩余用量和积分余额 — {values}

# zh-Hant
status_bar.grok.weekly_short = 週 {value}%
status_bar.grok.monthly_short = 月 {value}%
status_bar.grok.credits_short = {value}
status_bar.grok.accessibility = Grok 剩餘用量：{values}
status_bar.grok.hover = Grok 每週、每月剩餘用量和點數餘額 — {values}
```

- [ ] **Step 4: Grok 표시 문자열과 provider renderer를 구현한다**

`app.rs`에 fixed-point formatter와 localized composition을 추가한다.

```rust
fn format_grok_credits(credits: crate::grok_usage::GrokCredits) -> String {
    match credits.currency {
        crate::grok_usage::GrokCurrency::Usd => format!(
            "${}.{:02}",
            credits.minor_units / 100,
            credits.minor_units % 100
        ),
    }
}

fn grok_usage_labels(
    usage: crate::grok_usage::GrokUsage,
    catalog: &i18n::Catalog,
) -> (String, String, String) {
    let mut parts = Vec::with_capacity(3);
    if let Some(value) = usage.weekly_remaining_percent {
        parts.push(catalog.t("status_bar.grok.weekly_short", &[("value", &value.to_string())]));
    }
    if let Some(value) = usage.monthly_remaining_percent {
        parts.push(catalog.t("status_bar.grok.monthly_short", &[("value", &value.to_string())]));
    }
    if let Some(credits) = usage.credits_left {
        let value = format_grok_credits(credits);
        parts.push(catalog.t("status_bar.grok.credits_short", &[("value", &value)]));
    }
    let visible = parts.join(" · ");
    let accessibility = catalog.t(
        "status_bar.grok.accessibility",
        &[("values", visible.as_str())],
    );
    let hover = catalog.t("status_bar.grok.hover", &[("values", visible.as_str())]);
    (visible, accessibility, hover)
}
```

`top_provider_usage`에 `grok_usage: Option<GrokUsage>`와 `catalog: &i18n::Catalog`을
추가하고 내부 renderer를 정의한다.

```rust
fn grok_provider(
    ui: &mut egui::Ui,
    usage: crate::grok_usage::GrokUsage,
    catalog: &i18n::Catalog,
) {
    let (logo, _) = ui.allocate_exact_size(egui::vec2(14.5, 14.5), egui::Sense::hover());
    crate::ui::agent_terminal::paint_announcement_provider_logo(ui, logo, "Grok");
    let (visible, accessibility, hover) = grok_usage_labels(usage, catalog);
    let response = ui
        .label(egui::RichText::new(visible).size(13.0).strong())
        .on_hover_text(hover);
    let enabled = ui.is_enabled();
    response.widget_info(move || {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Label,
            enabled,
            accessibility,
        )
    });
}
```

표시 판정과 count에 Grok을 추가한다.

```rust
let grok_shown = agent_is_enabled(disabled, AgentKind::Grok) && grok_usage.is_some();
let visible_count = usize::from(claude_shown)
    + usize::from(codex_shown)
    + usize::from(kimi_shown)
    + usize::from(grok_shown);
```

Kimi block 뒤에 기존 `drawn` separator 규칙으로 Grok block을 추가하고
Kimi를 그린 직후 `drawn = true`로 갱신한다. 이어지는 Grok block은
`grok_provider(ui, grok_usage.expect("grok_shown requires usage"), catalog)`를 호출한다.
Claude/Codex가 꺼지고 Kimi/Grok만 보이는 경우에도 둘 사이 separator가 남는지 kittest로
확인한다.

- [ ] **Step 5: App이 감지된 executable만 worker에 전달하게 연결한다**

앱 시작 후 launcher detection을 한 번 수행해 렌더에서 NVM 디렉터리를 읽지 않게 한다.

```rust
agent_launcher_detection_requested: true,
```

상태바 렌더의 Kimi usage 다음에 다음 projection을 추가한다.

```rust
let grok_executable = self
    .agent_launcher_snapshot
    .as_ref()
    .and_then(|snapshot| snapshot.find(crate::agent_launcher::AgentKind::Grok))
    .map(crate::agent_launcher::DetectedAgent::executable);
let grok_usage = crate::agent_launcher::agent_is_enabled(
    &self.config.agents.disabled,
    crate::agent_launcher::AgentKind::Grok,
)
.then(|| crate::grok_usage::current(ui.ctx(), grok_executable))
.flatten();
```

`status_bar_with_managers` 인자 순서는
`claude_usage, codex_usage, codex_meta, kimi_usage, grok_usage, disabled_agents`로 고정한다.
함수 내부 `top_provider_usage` 호출에도 같은 순서와 마지막 `catalog`을 전달한다. 모든
기존 test 호출에는 Kimi 인자 다음에 `None`을 추가하고, App의 직접
`top_provider_usage` tests에는 Grok `None`과 fallback catalog를 전달한다.

- [ ] **Step 6: status/i18n/App wiring을 GREEN으로 만든다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo 사용량_바 --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo grok_usage --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p deppy-sijo --bin deppy-sijo ui::agent_terminal::tests --locked -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p i18n --locked -- --test-threads=1
cargo run -p xtask -- i18n-check
cargo run -p xtask -- check-boundary
cargo fmt --all -- --check
git diff --check
```

Expected: 모든 선택 test와 5-locale 검사가 PASS, boundary/fmt/diff exit 0.

- [ ] **Step 7: 상태바 wave를 기록하고 최종 리뷰를 위해 uncommitted로 유지한다**

```bash
git status --short
git diff --check
```

Expected: Task 5의 App/UI/locale/handoff 변경만 uncommitted로 남고 `git diff --check`는
exit 0이다. 앞선 Task 1~4 commits는 그대로 유지한다.

### Task 6: 전체 검증, Codex 리뷰, 서명 빌드, 재실행, Workstep 기록을 완료한다

**Files:**
- Modify: `docs/CODEX_HANDOFF.md`
- Create: Obsidian `프로젝트 일지/deppy-sijo/2026-08-25 Grok 런처와 잔여 사용량.md`

- [ ] **Step 1: 전체 serial test와 strict static gates를 실행한다**

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo --locked -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test -p i18n --locked -- --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo clippy -p i18n --all-targets --locked -- -D warnings
cargo run -p xtask -- i18n-check
cargo run -p xtask -- check-boundary
cargo fmt --all -- --check
git diff --check
```

Expected: 모든 command exit 0. 실패가 있으면 handoff에 정확한 command, exit code, 실패
test를 기록하고 그 실패를 고친 뒤 동일 command를 재실행한다.

- [ ] **Step 2: 사용자 계약을 보안·성능 관점으로 직접 대조한다**

```bash
rg -n "api_key|GROK_API_KEY|raw PTY|String::from_utf8_lossy" crates/app/src/agent_model_catalog.rs crates/app/src/grok_usage.rs crates/app/src/app.rs
rg -n "spawn\(|request_repaint|REFRESH_INTERVAL|PROBE_TIMEOUT|MAX_OUTPUT_BYTES|STALE_AFTER" crates/app/src/grok_usage.rs
rg -n "grok_usage::current|AgentKind::Grok|status_bar.grok" crates/app/src/app.rs crates/app/src/ui/agent_terminal.rs crates/i18n/locales/*/messages.txt
```

Expected: secret field 이름은 test fixture 또는 명시적인 비노출 설명에만 있고 production
projection/logging field에는 없다. spawn은 usage admission 한 곳, repaint는 worker 완료
한 곳, 모든 resource 상한은 상수로 확인된다.

- [ ] **Step 3: Workstep에 따라 전체 feature range와 uncommitted wave를 Codex CLI로 리뷰한다**

```bash
GROK_PLAN_BASE=$(git log -1 --format=%H -- docs/superpowers/plans/2026-08-25-grok-launcher-and-usage.md)
codex review --base "$GROK_PLAN_BASE"
codex review --uncommitted
```

Expected: 첫 리뷰는 plan commit 이후 Task 1~4 commits와 Task 5 working tree를 함께 보고,
둘째 리뷰는 아직 커밋하지 않은 Task 5 wave를 Workstep 계약대로 본다. actionable finding은
0건이어야 한다. finding이 있으면 정확한 위치와 재현을 handoff에 기록하고, 해당 동작을
실패시키는 focused regression test를 먼저 추가해 RED를 관찰한 뒤 최소 수정과 관련
group/전체 gate를 재실행한다. 리뷰 command가 5분을 넘겨 결론을 내지 않으면 중단 사실과
경과 시간을 기록하고 같은 범위의 수동 diff review를 수행하되 Codex 통과로 주장하지 않는다.

- [ ] **Step 4: 최종 correction과 handoff를 커밋한다**

```bash
git add crates/app/src/app.rs crates/app/src/ui/agent_terminal.rs crates/i18n/locales/ko-KR/messages.txt crates/i18n/locales/en-US/messages.txt crates/i18n/locales/ja-JP/messages.txt crates/i18n/locales/zh-Hans/messages.txt crates/i18n/locales/zh-Hant/messages.txt docs/CODEX_HANDOFF.md
git commit -m "feat(status): Grok 잔여 사용량 표시 추가"
```

리뷰 correction이 있으면 위 commit에 regression test와 최소 수정도 함께 포함한다.

- [ ] **Step 5: Developer ID로 release bundle을 재빌드하고 검증한다**

```bash
DEPPY_SIGN_IDENTITY='Developer ID Application: VectorNine INC (ZDTU5LS35K)' CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 sh scripts/package-macos.sh
codesign --verify --deep --strict --verbose=2 'target/bundle/Deppy Sijo.app'
unzip -tq 'target/bundle/Deppy Sijo.zip'
shasum -a 256 'target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo' 'target/bundle/Deppy Sijo.zip'
```

Expected: package script, deep/strict codesign, ZIP test 모두 exit 0. 출력한 SHA-256을
handoff와 Workstep 일지에 기록한다.

- [ ] **Step 6: 정확한 기존 bundle만 종료하고 새 bundle을 재실행한다**

먼저 read-only로 exact executable을 확인한다.

```bash
pgrep -fal '/target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo$'
```

표시된 PID의 `command`가 정확한 기존 bundle 경로와 일치할 때만 해당 PID에 SIGTERM을
보내고, 종료를 확인한 뒤 다음을 실행한다.

```bash
open -n 'target/bundle/Deppy Sijo.app'
pgrep -fal '/target/bundle/Deppy Sijo.app/Contents/MacOS/deppy-sijo$'
```

Expected: 새 process 한 개가 exact bundle path로 실행되고 반복 확인에서 살아 있다.

- [ ] **Step 7: 실제 UI 인수 기준을 확인한다**

런처에서 Grok 4.6/4.5와 모델별 강도 목록을 확인하고 4.6/medium으로 세션을 한 번
시작한다. 현재 CLI가 미인증이면 하단 Grok cell이 없는 것이 정상이다. 인증된 계정에서
확인 가능한 경우 `주/월/$` 세 값, partial 값, 비활성화 시 숨김을 확인한다. 사용량 갱신
동안 terminal 내용·cursor·split 화면이 깜빡이지 않는지 확인하고 관찰 결과만 기록한다.

- [ ] **Step 8: Obsidian Workstep 일지와 최종 handoff를 작성하고 커밋한다**

`workstep` 스킬을 다시 읽고 일지에 목표, 설계 결정, 실제 RED/GREEN command와 수치,
Codex review 결론, signed artifact hash, 재실행 PID, 미인증으로 확인하지 못한 항목을
사실대로 기록한다. `docs/CODEX_HANDOFF.md`에는 동일한 검증 결과와 exact next command를
갱신한다.

```bash
git add docs/CODEX_HANDOFF.md
git commit -m "docs: Grok 런처 작업 결과 기록"
git status --short --branch
git log -6 --oneline
```

Expected: repository worktree clean, branch ahead 상태와 마지막 feature/review/docs commits가
보인다. push는 사용자가 명시적으로 요청하기 전에는 실행하지 않는다.
