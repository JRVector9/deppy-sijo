//! 설치된 CLI가 디스크에 남기는 모델 카탈로그를 읽는다.
//!
//! Codex와 Kimi는 자기 모델 목록을 사용자 홈에 기계가 읽을 수 있는 형태로 남기고,
//! 그 파일은 CLI가 서버에서 갱신한다. 그래서 새 모델이 나와도 이 앱을 고칠 필요가
//! 없다. Claude Code와 Cursor에는 여기서 읽는 디스크 카탈로그가 없으므로
//! `agent_model_probe`가 별도 CLI 조회를 수행한다.
//!
//! 정상적인 빈 목록과 파일 삭제·읽기/파싱 실패를 구분한다. 호출자는 이 상태로
//! 폴백 여부를 결정한다. 파일 I/O를 하므로 렌더 스레드에서 호출하면 안 된다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;

use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::agent_launcher::{AgentKind, ModelChoice, ReasoningEffort};

/// 카탈로그 파일 읽기 상한. 현재 Codex 파일이 300KB대이고 모델마다 system prompt를
/// 통째로 담아 계속 커진다. 넘으면 파싱하지 않고 내장 목록으로 폴백한다.
const CATALOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 드롭다운에 넣을 모델 수 상한. Kimi는 `kimi provider add <models.dev>`로
/// `[models.*]`가 수백~수천 개까지 불어날 수 있어 상한이 반드시 필요하다.
const CATALOG_MODELS_MAX: usize = 64;
const GROK_PARSED_EFFORT_ORDER: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];

/// 이 종류의 에이전트가 디스크 카탈로그를 갖는지. 외부 CLI 조회와는 별개다.
pub(crate) const fn has_disk_catalog(kind: AgentKind) -> bool {
    matches!(
        kind,
        AgentKind::Codex | AgentKind::Kimi | AgentKind::Grok | AgentKind::QwenCode
    )
}

/// 빈 목록도 정상 결과다. 실패와 구분해야 갱신 시 이전 목록의 보존 여부를 판단할 수 있다.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CatalogLoad {
    Ready(Vec<ModelChoice>),
    Missing,
    Unavailable,
    Unsupported,
}

/// 디스크 카탈로그를 한 번 읽고 파싱한다. 원문이나 읽기 오류의 경로는 결과에 담지 않는다.
pub(crate) fn load(
    kind: AgentKind,
    home: Option<&Path>,
    configured_default: Option<&str>,
) -> CatalogLoad {
    if !has_disk_catalog(kind) {
        return CatalogLoad::Unsupported;
    }
    let Some(home) = home else {
        return CatalogLoad::Unavailable;
    };
    let relative = match kind {
        AgentKind::Codex => ".codex/models_cache.json",
        AgentKind::Kimi => ".kimi-code/config.toml",
        AgentKind::Grok => ".grok/models_cache.json",
        AgentKind::QwenCode => ".qwen/settings.json",
        _ => return CatalogLoad::Unsupported,
    };
    let text = match read_bounded_result(&home.join(relative)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return CatalogLoad::Missing,
        Err(_) => return CatalogLoad::Unavailable,
    };
    let models = match kind {
        AgentKind::Codex => decode_codex(&text),
        AgentKind::Kimi => decode_kimi(&text),
        AgentKind::Grok => decode_grok(&text, configured_default),
        AgentKind::QwenCode => decode_qwen(&text),
        _ => return CatalogLoad::Unsupported,
    };
    match models {
        Ok(models) => CatalogLoad::Ready(models),
        Err(()) => CatalogLoad::Unavailable,
    }
}

/// CLI가 자기 설정 파일에 적어 둔 기본 모델. 런처에서 "기본 모델" 자리표시자를 없앤 대신
/// 이 값을 미리 선택해, 앱으로 띄운 결과가 CLI를 그냥 실행한 것과 같게 유지한다.
pub(crate) fn configured_default_model(kind: AgentKind, home: Option<&Path>) -> Option<String> {
    let home = home?;
    match kind {
        AgentKind::Codex => codex_configured_default_model(home),
        AgentKind::Kimi => kimi_configured_default_model(home, kimi_model_name_env().as_deref()),
        AgentKind::Claude => claude_configured_default_model(home),
        AgentKind::Grok => grok_configured_defaults(Some(home)).0,
        AgentKind::QwenCode => qwen_configured_default_model(home),
        _ => None,
    }
}

/// `KIMI_MODEL_NAME` 조회를 이 얇은 래퍼에만 가둬, 나머지 로직은 값을 인자로 받는
/// 순수 함수로 남긴다 — 테스트가 프로세스 환경변수를 건드리지 않아도 되게 하기 위함이다.
fn kimi_model_name_env() -> Option<String> {
    std::env::var("KIMI_MODEL_NAME").ok()
}

/// `~/.codex/config.toml`의 최상위 `model` 키. `[projects."..."]` 하위 테이블은
/// `trust_level`만 담고 모델 정보가 없으므로 건드리지 않는다.
#[derive(Deserialize)]
struct CodexTopLevelConfig {
    #[serde(default)]
    model: Option<String>,
}

fn parse_codex_default_model(text: &str) -> Option<String> {
    let config: CodexTopLevelConfig = toml::from_str(text).ok()?;
    trimmed_non_empty(config.model)
}

fn codex_configured_default_model(home: &Path) -> Option<String> {
    let text = read_bounded(&home.join(".codex/config.toml"))?;
    parse_codex_default_model(&text)
}

/// `~/.kimi-code/config.toml`의 최상위 `default_model` 키.
#[derive(Deserialize)]
struct KimiDefaultModelConfig {
    #[serde(default)]
    default_model: Option<String>,
}

fn parse_kimi_default_model(text: &str) -> Option<String> {
    let config: KimiDefaultModelConfig = toml::from_str(text).ok()?;
    trimmed_non_empty(config.default_model)
}

/// `KIMI_MODEL_NAME`이 설정돼 있으면 Kimi가 그 값으로 인메모리 모델을 합성한다 —
/// 디스크에 대응하는 슬러그가 없으므로 안다고 주장하지 않고 `None`을 돌려준다.
fn kimi_configured_default_model(home: &Path, kimi_model_name_env: Option<&str>) -> Option<String> {
    if kimi_model_name_env.is_some_and(|value| !value.trim().is_empty()) {
        return None;
    }
    let text = read_bounded(&home.join(".kimi-code/config.toml"))?;
    parse_kimi_default_model(&text)
}

/// `~/.claude/settings.json`의 최상위 `"model"` 키.
#[derive(Deserialize)]
struct ClaudeSettingsFile {
    #[serde(default)]
    model: Option<String>,
    /// `/effort <level>`이 "saved as your default for new sessions"로 여기에 쓴다.
    #[serde(default, rename = "effortLevel")]
    effort_level: Option<String>,
}

#[cfg(test)]
fn parse_claude_default_model(text: &str) -> Option<String> {
    parse_claude_defaults(text).0
}

fn claude_configured_default_model(home: &Path) -> Option<String> {
    claude_configured_defaults(Some(home)).0
}

#[cfg(test)]
fn parse_claude_default_effort(text: &str) -> Option<String> {
    parse_claude_defaults(text).1
}

fn parse_claude_defaults(text: &str) -> (Option<String>, Option<String>) {
    let Some(settings) = serde_json::from_str::<ClaudeSettingsFile>(text).ok() else {
        return (None, None);
    };
    (
        trimmed_non_empty(settings.model),
        trimmed_non_empty(settings.effort_level),
    )
}

/// `~/.claude/settings.json`의 `effortLevel` — Claude가 `/effort`로 저장하는 전역
/// 기본값이다.
///
/// 세션의 현재 강도를 아는 마지막 수단이다. statusLine은 1시간이면 만료되고
/// (`STATUSLINES_PREFIX_PREFLIGHT`), argv는 **런처로 띄웠을 때만** 값이 있다 —
/// 사용자가 셸에 `claude`라고 직접 치면 인자가 비어 있다(2026-08-03 실증). 그 경우
/// 새 세션은 이 전역 기본값으로 시작하므로 이 값이 곧 현재 강도다.
/// Claude 설정 파일을 한 번만 읽고 모델·강도 기본값을 같이 반환한다.
/// 런처 감지 worker가 한 스냅샷에 두 값을 저장할 때 같은 JSON을 두 번
/// 열고 파싱하지 않게 하는 경계다.
pub(crate) fn claude_configured_defaults(home: Option<&Path>) -> (Option<String>, Option<String>) {
    let Some(home) = home else {
        return (None, None);
    };
    let Some(text) = read_bounded(&home.join(".claude/settings.json")) else {
        return (None, None);
    };
    parse_claude_defaults(&text)
}

/// `~/.grok/config.toml`의 `[models]` 테이블 아래 `default` 키. Codex의 `model`과 달리
/// 최상위가 아니라 중첩 테이블이므로 최상위에 같은 이름의 키가 있어도 무시한다.
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

/// `~/.qwen/settings.json`의 중첩 `model.name` 키.
fn parse_qwen_default_model(text: &str) -> Option<String> {
    let settings: QwenSettings = serde_json::from_str(text).ok()?;
    trimmed_non_empty(settings.model.and_then(|model| model.name))
}

fn qwen_configured_default_model(home: &Path) -> Option<String> {
    let text = read_bounded(&home.join(".qwen/settings.json"))?;
    parse_qwen_default_model(&text)
}

/// 값을 다듬고 빈 문자열/공백뿐인 문자열은 버린다. 세 CLI 리더가 공유하는 마지막 정리
/// 단계다.
fn trimmed_non_empty(value: Option<String>) -> Option<String> {
    let trimmed = value?.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// 상한 안의 UTF-8 일반 파일만 읽는다. 없음/과대/비UTF-8/권한 오류는 모두 `None`이다.
fn read_bounded(path: &Path) -> Option<String> {
    read_bounded_result(path).ok()
}

fn read_bounded_result(path: &Path) -> io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > CATALOG_MAX_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    read_limited(file)
}

fn read_limited(reader: impl Read) -> io::Result<String> {
    let mut bytes = Vec::new();
    // metadata 확인 뒤 파일이 커져도 상한보다 한 바이트만 더 읽고 중단한다.
    reader.take(CATALOG_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CATALOG_MAX_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    String::from_utf8(bytes).map_err(|_| io::ErrorKind::InvalidData.into())
}

fn decode_json_object<'de, T: Deserialize<'de>>(text: &'de str) -> Result<T, ()> {
    // serde 구조체는 배열 표현도 받지만 CLI 카탈로그의 루트 계약은 객체다.
    if !text.trim_start().starts_with('{') {
        return Err(());
    }
    serde_json::from_str(text).map_err(|_| ())
}

#[derive(Deserialize)]
struct CodexCache {
    // `CodexModel`은 아니고 관대한 `serde_json::Value`로 받는다 — 항목 하나가 스키마에
    // 안 맞아도(예: `slug` 누락) 배열 전체의 역직렬화가 실패하지 않게 하기 위함이다.
    // 개별 변환은 `decode_codex`에서 항목별로 시도하고 실패한 항목만 건너뛴다.
    models: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CodexModel {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevel>,
}

#[derive(Deserialize)]
struct CodexReasoningLevel {
    effort: String,
}

/// `~/.codex/models_cache.json`. `visibility`는 서버가 정하는 열린 문자열이라
/// 아는 값만 통과시킨다 — 모델 선택기에 새 내부 모델이 새는 것보다 안 보이는 쪽이 낫다.
fn decode_codex(text: &str) -> Result<Vec<ModelChoice>, ()> {
    let cache: CodexCache = decode_json_object(text)?;
    let mut models: Vec<(i64, ModelChoice)> = cache
        .models
        .into_iter()
        // `slug`처럼 필수 필드가 빠진 항목 하나 때문에 카탈로그 전체가 사라지면 안 된다
        // — 그 항목만 건너뛰고 나머지는 그대로 살린다.
        .filter_map(|value| serde_json::from_value::<CodexModel>(value).ok())
        .filter(|model| model.visibility.as_deref() == Some("list"))
        .filter_map(|model| {
            let efforts = collect_efforts(
                model
                    .supported_reasoning_levels
                    .iter()
                    .map(|level| level.effort.as_str()),
            );
            let default_effort = model
                .default_reasoning_level
                .as_deref()
                .and_then(effort_from_value);
            let priority = model.priority.unwrap_or(i64::MAX);
            let label = model.display_name.as_deref().unwrap_or(&model.slug);
            ModelChoice::new(&model.slug, label, efforts, default_effort)
                .map(|choice| (priority, choice))
        })
        .collect();
    // 카탈로그의 `priority`가 Codex 자신의 모델 선택기 순서다. 동순위는 파일 순서를
    // 유지한다.
    models.sort_by_key(|(priority, _)| *priority);
    models.truncate(CATALOG_MODELS_MAX);
    Ok(models.into_iter().map(|(_, choice)| choice).collect())
}

#[derive(Deserialize)]
struct KimiConfigRaw {
    #[serde(default)]
    default_model: Option<String>,
    // `KimiModel`이 아니라 관대한 `toml::Value`로 받는다 — 항목 하나의 필드 타입이
    // 스키마와 안 맞아도(예: `support_efforts`가 배열이 아님) 테이블 전체의 역직렬화가
    // 실패하지 않게 하기 위함이다. 개별 변환은 `decode_kimi`에서 항목별로 시도하고
    // 실패한 항목만 건너뛴다.
    #[serde(default)]
    models: BTreeMap<String, toml::Value>,
}

/// Kimi의 `[models.*]`는 스키마상 모든 키가 optional이고 모르는 키를 허용한다.
#[derive(Deserialize)]
struct KimiModel {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    capabilities: Option<toml::Value>,
    #[serde(default)]
    support_efforts: Option<Vec<String>>,
    #[serde(default)]
    default_effort: Option<String>,
}

/// `~/.kimi-code/config.toml`. `support_efforts`를 선언하지 않았지만 thinking을
/// 지원하는 모델은 Kimi 자신이 boolean 모델로 다루며 값이 `on`/`off` 둘뿐이다.
fn decode_kimi(text: &str) -> Result<Vec<ModelChoice>, ()> {
    let raw = toml::from_str::<KimiConfigRaw>(text).map_err(|_| ())?;
    // `models`는 `BTreeMap`이라 별칭 알파벳 순으로 나온다. 이 저장소가 쓰는 `toml`
    // 크레이트(1.1, 기본 feature)는 `preserve_order`가 꺼져 있어 `toml::Value::Table`도
    // 문서 순서를 보존하지 않는다 — 직접 확인함(별도 프로브 테스트로 뒤섞인 문서 순서가
    // 알파벳 순으로 나오는 것을 확인했다). 그래서 파일 순서를 복원할 방법이 없고, 대신
    // `default_model`이 가리키는 별칭만은 잘림 창 밖으로 밀려도 살려낸다.
    let default_alias = trimmed_non_empty(raw.default_model);
    let models: Vec<ModelChoice> = raw
        .models
        .into_iter()
        .filter_map(|(alias, value)| {
            // 이 항목 하나가 스키마에 안 맞으면 이 모델만 건너뛰고 나머지는 살린다.
            let model: KimiModel = value.try_into().ok()?;
            let declared = model
                .support_efforts
                .as_ref()
                .map(|efforts| collect_efforts(efforts.iter().map(String::as_str)))
                .unwrap_or_default();
            let efforts = if !declared.is_empty() {
                declared
            } else if supports_thinking(model.capabilities.as_ref()) {
                vec![ReasoningEffort::On, ReasoningEffort::Off]
            } else {
                Vec::new()
            };
            let default_effort = model.default_effort.as_deref().and_then(effort_from_value);
            let label = model.display_name.as_deref().unwrap_or(&alias);
            ModelChoice::new(&alias, label, efforts, default_effort)
        })
        .collect();
    Ok(truncate_keeping_alias(models, default_alias.as_deref()))
}

/// 목록이 상한을 넘으면 자르되, `keep_value`로 지정된 항목이 잘림 창 밖에 있으면
/// 마지막 자리에 끼워 넣어 항상 살아남게 한다. Kimi의 `[models.*]`는 알파벳 순으로
/// 도착하므로, `kimi provider add`로 수백 개가 쌓이면 알파벳상 뒤에 있는 사용자의 실제
/// 기본 모델(`default_model`)이 그냥 자르는 것만으로는 사라져 버린다.
fn truncate_keeping_alias(
    mut models: Vec<ModelChoice>,
    keep_value: Option<&str>,
) -> Vec<ModelChoice> {
    if models.len() <= CATALOG_MODELS_MAX {
        return models;
    }
    let keep_pos =
        keep_value.and_then(|value| models.iter().position(|model| model.value() == value));
    match keep_pos {
        Some(pos) if pos >= CATALOG_MODELS_MAX => {
            let kept = models.remove(pos);
            models.truncate(CATALOG_MODELS_MAX - 1);
            models.push(kept);
        }
        _ => models.truncate(CATALOG_MODELS_MAX),
    }
    models
}

/// `capabilities`는 배열이거나 테이블일 수 있다. 어느 쪽이든 thinking 지원 여부만 본다.
fn supports_thinking(capabilities: Option<&toml::Value>) -> bool {
    match capabilities {
        Some(toml::Value::Array(items)) => items.iter().any(|item| {
            item.as_str().is_some_and(|name| {
                let name = name.trim().to_ascii_lowercase();
                name == "thinking" || name == "always_thinking"
            })
        }),
        Some(toml::Value::Table(table)) => table
            .get("thinking")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

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

struct GrokCacheEntry {
    info: Option<serde_json::Value>,
}

impl<'de> Deserialize<'de> for GrokCacheEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct GrokCacheEntryVisitor;

        impl<'de> Visitor<'de> for GrokCacheEntryVisitor {
            type Value = GrokCacheEntry;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Grok model cache entry")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut info = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "info" {
                        info = Some(map.next_value()?);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(GrokCacheEntry { info })
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_borrowed_str<E>(self, _value: &'de str) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(GrokCacheEntry { info: None })
            }
        }

        deserializer.deserialize_any(GrokCacheEntryVisitor)
    }
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

#[derive(Deserialize)]
struct GrokReasoningEffort {
    value: String,
    #[serde(default)]
    default: bool,
}

fn grok_model_choice(value: serde_json::Value, fallback_id: Option<String>) -> Option<ModelChoice> {
    let model = serde_json::from_value::<GrokModel>(value).ok()?;
    if model.hidden || model.supported_in_api == Some(false) {
        return None;
    }
    let id = trimmed_non_empty(model.id).or_else(|| trimmed_non_empty(fallback_id))?;
    let efforts = if model.supports_reasoning_effort {
        collect_grok_efforts(
            model
                .reasoning_efforts
                .iter()
                .map(|effort| effort.value.as_str()),
        )
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

fn push_unique_model(
    models: &mut Vec<ModelChoice>,
    seen: &mut HashSet<String>,
    model: ModelChoice,
) {
    if models.len() < CATALOG_MODELS_MAX && seen.insert(model.value().to_owned()) {
        models.push(model);
    }
}

/// `~/.grok/models_cache.json`. Codex와 달리 `priority` 필드가 없으므로 정렬하지 않고
/// 카탈로그 배열 순서를 그대로 쓴다.
fn decode_grok(text: &str, configured_default: Option<&str>) -> Result<Vec<ModelChoice>, ()> {
    let cache: GrokCache = decode_json_object(text)?;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    match cache.models {
        GrokModelCollection::Array(values) => {
            for value in values {
                if models.len() >= CATALOG_MODELS_MAX {
                    break;
                }
                if let Some(model) = grok_model_choice(value, None) {
                    push_unique_model(&mut models, &mut seen, model);
                }
            }
        }
        GrokModelCollection::Object(values) => {
            let mut promoted_key = None;
            if let Some(default) = configured_default
                && let Some(entry) = values.get(default)
                && let Some(info) = entry.info.as_ref().cloned()
                && let Some(model) = grok_model_choice(info, Some(default.to_owned()))
                && model.value() == default
            {
                push_unique_model(&mut models, &mut seen, model);
                promoted_key = Some(default);
            }
            for (id, entry) in values {
                if promoted_key == Some(id.as_str()) {
                    continue;
                }
                if models.len() >= CATALOG_MODELS_MAX {
                    break;
                }
                if let Some(info) = entry.info
                    && let Some(model) = grok_model_choice(info, Some(id))
                {
                    push_unique_model(&mut models, &mut seen, model);
                }
            }
        }
    }
    Ok(models)
}

/// `~/.qwen/settings.json`. Qwen은 서버 카탈로그를 내려받지 않고, 사용자가 손으로 등록한
/// `modelProviders.<authType>` 배열만 모델 후보가 된다. Qwen Code는 `openai`/`anthropic`/
/// `qwen-oauth`/`gemini`/`vertex-ai` 외에도 사용자가 등록한 커스텀 provider id(예:
/// "idealab")를 허용하므로, 고정된 다섯 필드가 아니라 임의의 키를 받는 맵으로 둔다.
#[derive(Deserialize)]
struct QwenSettings {
    #[serde(default)]
    model: Option<QwenModelSetting>,
    // `QwenModelProviders`(다섯 개 고정 필드) 대신 관대한 맵으로 받는다: 이래야 커스텀
    // provider id도 보이고(결함 4), provider 하나 안의 모델 항목 하나가 스키마에 안
    // 맞아도(결함 1) 그 항목만 걸러내고 나머지는 살릴 수 있다.
    #[serde(default, rename = "modelProviders")]
    model_providers: Option<BTreeMap<String, Vec<serde_json::Value>>>,
}

#[derive(Deserialize)]
struct QwenModelSetting {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct QwenProviderModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
}

/// 이전에 고정 필드로 두던 다섯 개 authType의 원래 순서. 실제 설정 파일에서 이 순서가
/// dedup 승자를 정하므로(먼저 나온 쪽이 이김), 커스텀 provider를 더 받도록 맵으로
/// 바꾸면서도 이 다섯 개의 상대 순서는 그대로 유지한다.
const QWEN_KNOWN_PROVIDER_ORDER: [&str; 5] =
    ["openai", "anthropic", "qwen-oauth", "gemini", "vertex-ai"];

/// Qwen은 reasoning-effort CLI 플래그가 없으므로 모든 모델이 빈 강도 목록이다. 알려진
/// 다섯 provider를 먼저 원래 순서대로, 그 외 커스텀 provider는 남은 키를 (BTreeMap
/// 순회이므로) 알파벳 순으로 이어 붙여 훑는다. 진짜 중복은 `(id, baseUrl)` 쌍이 모두
/// 같을 때뿐이다 — Qwen Code 문서상 같은 id라도 baseUrl이 다르면 별개 모델이다.
fn decode_qwen(text: &str) -> Result<Vec<ModelChoice>, ()> {
    let settings: QwenSettings = decode_json_object(text)?;
    let Some(mut providers) = settings.model_providers else {
        return Ok(Vec::new());
    };

    let mut ordered_entries: Vec<serde_json::Value> = Vec::new();
    for key in QWEN_KNOWN_PROVIDER_ORDER {
        if let Some(models) = providers.remove(key) {
            ordered_entries.extend(models);
        }
    }
    for (_, models) in providers {
        ordered_entries.extend(models);
    }

    let mut seen = HashSet::new();
    let retained: Vec<QwenProviderModel> = ordered_entries
        .into_iter()
        // 이 항목 하나가 스키마에 안 맞아도(예: `id` 누락) 이 모델만 건너뛰고 나머지는
        // 살린다.
        .filter_map(|value| serde_json::from_value::<QwenProviderModel>(value).ok())
        .filter(|model| seen.insert((model.id.clone(), model.base_url.clone())))
        .collect();

    // 같은 id가 서로 다른 baseUrl로 남아 있으면 라벨이 겹쳐 선택기에서 구분이 안 되므로,
    // 그럴 때만 baseUrl의 호스트를 라벨에 덧붙인다. `retained`를 뒤에서 그대로 소비해야
    // 하므로 카운트 맵은 `retained`를 빌리지 않게 키를 복제해 소유한다.
    let mut id_counts: HashMap<String, usize> = HashMap::new();
    for model in &retained {
        *id_counts.entry(model.id.clone()).or_insert(0) += 1;
    }

    Ok(retained
        .into_iter()
        .filter_map(|model| {
            let ambiguous = id_counts
                .get(model.id.as_str())
                .is_some_and(|count| *count > 1);
            let label = qwen_label(&model, ambiguous);
            ModelChoice::new(&model.id, &label, Vec::new(), None)
        })
        .take(CATALOG_MODELS_MAX)
        .collect())
}

/// 표시 라벨을 만든다. 같은 id가 여러 baseUrl로 남아 있을 때만(ambiguous) baseUrl의
/// 호스트를 덧붙여 선택기에서 서로 구분되게 한다.
fn qwen_label(model: &QwenProviderModel, ambiguous: bool) -> String {
    let base_label = model.name.as_deref().unwrap_or(model.id.as_str());
    if !ambiguous {
        return base_label.to_string();
    }
    match model.base_url.as_deref().map(str::trim) {
        Some(base_url) if !base_url.is_empty() => {
            format!("{base_label} ({})", qwen_host_label(base_url))
        }
        _ => base_label.to_string(),
    }
}

/// `baseUrl`에서 호스트만 뽑아 라벨에 붙일 짧은 문자열로 쓴다. URL 파싱 의존성을 새로
/// 들이지 않는 최소 구현이라, 스킴이 없거나 슬래시가 없으면 원문을 그대로 쓴다.
fn qwen_host_label(base_url: &str) -> &str {
    let without_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

/// 아는 강도만 남긴다. 모르는 값은 조용히 버려 새 단계가 생겨도 파싱이 깨지지 않는다.
fn collect_efforts<'a>(values: impl Iterator<Item = &'a str>) -> Vec<ReasoningEffort> {
    let mut efforts = Vec::new();
    for effort in values.filter_map(effort_from_value) {
        if !efforts.contains(&effort) {
            efforts.push(effort);
        }
    }
    efforts
}

fn collect_grok_efforts<'a>(values: impl Iterator<Item = &'a str>) -> Vec<ReasoningEffort> {
    let available = collect_efforts(values);
    GROK_PARSED_EFFORT_ORDER
        .iter()
        .copied()
        .filter(|effort| available.contains(effort))
        .collect()
}

pub(crate) fn effort_from_value(value: &str) -> Option<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::XHigh),
        "max" => Some(ReasoningEffort::Max),
        "ultra" => Some(ReasoningEffort::Ultra),
        "on" => Some(ReasoningEffort::On),
        "off" => Some(ReasoningEffort::Off),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// `/effort`가 저장하는 전역 기본값을 읽는다. statusLine은 1시간이면 만료되고
    /// argv는 런처로 띄웠을 때만 값이 있어서, 사용자가 셸에 `claude`라고 직접 친
    /// 세션에서는 이게 현재 강도를 아는 유일한 근거다.
    #[test]
    fn claude_settings에서_전역_기본_강도를_읽는다() {
        // 사용자 환경의 실제 형태.
        let text = r#"{"model":"opus[1m]","effortLevel":"xhigh","other":1}"#;
        assert_eq!(
            super::parse_claude_default_effort(text).as_deref(),
            Some("xhigh")
        );
        // 모델 파싱과 서로 간섭하지 않는다.
        assert_eq!(
            super::parse_claude_default_model(text).as_deref(),
            Some("opus[1m]")
        );
        // 키가 없거나 비면 None — 임의 값을 지어내지 않는다.
        assert_eq!(
            super::parse_claude_default_effort(r#"{"model":"opus"}"#),
            None
        );
        assert_eq!(
            super::parse_claude_default_effort(r#"{"effortLevel":"  "}"#),
            None
        );
        assert_eq!(super::parse_claude_default_effort("not json"), None);
    }

    use super::*;

    // 기존 항목 변환 fixture는 유지하고, 성공/실패 구분은 아래 load 회귀에서 따로 검증한다.
    fn parse_codex(text: &str) -> Vec<ModelChoice> {
        decode_codex(text).unwrap_or_default()
    }

    fn parse_kimi(text: &str) -> Vec<ModelChoice> {
        decode_kimi(text).unwrap_or_default()
    }

    fn parse_grok(text: &str, configured: Option<&str>) -> Vec<ModelChoice> {
        decode_grok(text, configured).unwrap_or_default()
    }

    fn parse_qwen(text: &str) -> Vec<ModelChoice> {
        decode_qwen(text).unwrap_or_default()
    }

    #[test]
    fn catalog_refresh_distinguishes_missing_invalid_and_empty_for_each_provider() {
        let home = unique_temp_dir("catalog-refresh-states");
        for (kind, relative, empty, malformed) in [
            (
                AgentKind::Codex,
                ".codex/models_cache.json",
                r#"{"models":[]}"#,
                "{}",
            ),
            (
                AgentKind::Grok,
                ".grok/models_cache.json",
                r#"{"models":{}}"#,
                r#"{"models":null}"#,
            ),
            (
                AgentKind::Kimi,
                ".kimi-code/config.toml",
                "[models]",
                "models = 42",
            ),
            (
                AgentKind::QwenCode,
                ".qwen/settings.json",
                r#"{"modelProviders":{}}"#,
                r#"{"modelProviders":42}"#,
            ),
        ] {
            let path = home.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            assert_eq!(
                load(kind, Some(&home), None),
                CatalogLoad::Missing,
                "{kind:?}"
            );
            for broken in ["{", malformed, "[]", "[[]]", "null", "42"] {
                std::fs::write(&path, broken).unwrap();
                assert_eq!(
                    load(kind, Some(&home), None),
                    CatalogLoad::Unavailable,
                    "{kind:?}"
                );
            }
            std::fs::write(&path, empty).unwrap();
            assert_eq!(
                load(kind, Some(&home), None),
                CatalogLoad::Ready(Vec::new()),
                "{kind:?}"
            );
            std::fs::write(&path, [0xff, 0xfe]).unwrap();
            assert_eq!(
                load(kind, Some(&home), None),
                CatalogLoad::Unavailable,
                "{kind:?}"
            );
            std::fs::remove_file(path).unwrap();
            assert_eq!(
                load(kind, Some(&home), None),
                CatalogLoad::Missing,
                "{kind:?}"
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn catalog_refresh_accepts_new_grok_ids_with_declared_efforts() {
        let home = unique_temp_dir("catalog-refresh-grok");
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(home.join(".grok/models_cache.json"), r#"{
            "models": {
                "grok-4.7": {"info": {"name":"Grok 4.7", "supported_in_api":true,
                    "supports_reasoning_effort":true, "reasoning_efforts":[{"value":"high","default":true}]}},
                "grok-4.7-build-fast": {"info": {"name":"Grok 4.7 Fast", "supported_in_api":true}}
            }
        }"#).unwrap();
        let CatalogLoad::Ready(models) = load(AgentKind::Grok, Some(&home), Some("grok-4.7"))
        else {
            panic!("정상 카탈로그를 읽어야 한다");
        };
        assert_eq!(values(&models), ["grok-4.7", "grok-4.7-build-fast"]);
        assert_eq!(models[0].efforts(), &[ReasoningEffort::High]);
        assert_eq!(models[0].default_effort(), Some(ReasoningEffort::High));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn catalog_refresh_limits_reads_even_when_input_exceeds_prior_metadata() {
        let mut growing = io::repeat(b' ').take(CATALOG_MAX_BYTES * 2);
        assert_eq!(
            read_limited(&mut growing).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(growing.limit(), CATALOG_MAX_BYTES - 1);
        let boundary = io::repeat(b' ').take(CATALOG_MAX_BYTES);
        assert_eq!(
            read_limited(boundary).unwrap().len() as u64,
            CATALOG_MAX_BYTES
        );
    }

    const CODEX_FIXTURE: &str = r#"{
      "fetched_at": "2026-07-31T22:50:01.441741Z",
      "etag": "W/\"abc\"",
      "client_version": "0.145.0",
      "models": [
        {
          "slug": "gpt-5.5",
          "display_name": "GPT-5.5",
          "visibility": "list",
          "priority": 7,
          "default_reasoning_level": "medium",
          "supported_reasoning_levels": [
            {"effort": "low", "description": "x"},
            {"effort": "medium", "description": "x"},
            {"effort": "high", "description": "x"},
            {"effort": "xhigh", "description": "x"}
          ]
        },
        {
          "slug": "gpt-5.6-sol",
          "display_name": "GPT-5.6-Sol",
          "visibility": "list",
          "priority": 1,
          "default_reasoning_level": "low",
          "supported_reasoning_levels": [
            {"effort": "low", "description": "x"},
            {"effort": "ultra", "description": "x"},
            {"effort": "brand-new-tier", "description": "x"}
          ]
        },
        {
          "slug": "codex-auto-review",
          "display_name": "Codex Auto Review",
          "visibility": "hide",
          "priority": 3,
          "default_reasoning_level": "medium",
          "supported_reasoning_levels": [{"effort": "low", "description": "x"}]
        }
      ]
    }"#;

    const KIMI_FIXTURE: &str = r#"
default_model = "kimi-code/kimi-for-coding"

[thinking]
enabled = true
effort = "high"

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
model = "kimi-for-coding"
max_context_size = 262144
capabilities = [ "thinking", "always_thinking", "image_in", "tool_use" ]
display_name = "K2.7 Coding"

[models."kimi-code/k3"]
provider = "managed:kimi-code"
model = "k3"
capabilities = [ "thinking", "always_thinking", "tool_use" ]
display_name = "K3"
support_efforts = [ "low", "high", "max" ]
default_effort = "high"

[models."zzz-no-thinking/plain"]
provider = "custom"
model = "plain"
capabilities = [ "tool_use" ]
display_name = "Plain"
"#;

    const GROK_FIXTURE: &str = r#"{
      "default": "grok-4.5",
      "models": [
        {
          "id": "grok-4.5",
          "name": "Grok 4.5",
          "supports_reasoning_effort": true,
          "reasoning_effort": "high",
          "reasoning_efforts": [
            { "value": "high",   "label": "High Effort",   "default": true },
            { "value": "medium", "label": "Medium Effort" },
            { "value": "low",    "label": "Low Effort" }
          ]
        },
        {
          "id": "grok-4-fast",
          "name": "Grok 4 Fast",
          "supports_reasoning_effort": true,
          "reasoning_effort": "medium",
          "reasoning_efforts": [
            { "value": "medium",  "label": "Medium Effort" },
            { "value": "minimal", "label": "Minimal Effort" }
          ]
        },
        {
          "id": "grok-3",
          "name": "Grok 3",
          "supports_reasoning_effort": false,
          "reasoning_efforts": [
            { "value": "high", "label": "High Effort", "default": true }
          ]
        },
        {
          "id": "grok-3-mini"
        }
      ]
    }"#;

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

    const QWEN_FIXTURE: &str = r#"{
      "model": { "name": "qwen3-coder-plus" },
      "modelProviders": {
        "openai": [
          { "id": "gpt-4o", "name": "GPT-4o" },
          { "id": "shared-id", "name": "OpenAI Shared" }
        ],
        "anthropic": [
          { "id": "shared-id", "name": "Anthropic Shared" },
          { "id": "claude-sonnet", "name": "Claude Sonnet" }
        ]
      }
    }"#;

    /// 테스트마다 다른 경로를 쓴다. 고정 이름이면 같은 호스트에서 테스트 바이너리가
    /// 두 번 동시에 돌 때 서로의 픽스처를 덮어쓴다. `agent_launcher`의 테스트들이 쓰는
    /// 방식과 같다.
    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deppy-catalog-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    fn values(models: &[ModelChoice]) -> Vec<&str> {
        models.iter().map(ModelChoice::value).collect()
    }

    #[test]
    fn codex_catalog_keeps_listed_models_in_picker_order() {
        let models = parse_codex(CODEX_FIXTURE);
        // `hide`인 codex-auto-review는 빠지고, priority 순으로 정렬된다.
        assert_eq!(values(&models), ["gpt-5.6-sol", "gpt-5.5"]);
        assert_eq!(models[0].label(), "GPT-5.6-Sol");
        assert_eq!(
            models[1].efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ]
        );
        assert_eq!(models[1].default_effort(), Some(ReasoningEffort::Medium));
    }

    #[test]
    fn codex_catalog_drops_unknown_effort_tiers_without_failing() {
        let models = parse_codex(CODEX_FIXTURE);
        assert_eq!(
            models[0].efforts(),
            [ReasoningEffort::Low, ReasoningEffort::Ultra]
        );
        assert_eq!(models[0].default_effort(), Some(ReasoningEffort::Low));
    }

    #[test]
    fn codex_catalog_survives_one_entry_missing_required_slug() {
        // 결함 1 회귀: `slug`가 없는 항목 하나 때문에 배열 전체(=전체 카탈로그)가
        // 사라지면 안 된다. 고치기 전에는 `serde_json::from_str::<CodexCache>` 자체가
        // 실패해 `parse_codex`가 빈 벡터를 돌려줬다.
        let json = r#"{"models":[
            {"display_name":"Missing Slug","visibility":"list","priority":1,"supported_reasoning_levels":[]},
            {"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list","priority":2,"supported_reasoning_levels":[]}
        ]}"#;
        assert_eq!(values(&parse_codex(json)), ["gpt-5.5"]);
    }

    #[test]
    fn kimi_catalog_maps_declared_efforts_and_boolean_thinking() {
        let models = parse_kimi(KIMI_FIXTURE);
        let by_value = |value: &str| {
            models
                .iter()
                .find(|model| model.value() == value)
                .unwrap_or_else(|| panic!("{value} missing"))
        };

        let k3 = by_value("kimi-code/k3");
        assert_eq!(k3.label(), "K3");
        assert_eq!(
            k3.efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(k3.default_effort(), Some(ReasoningEffort::High));

        // support_efforts가 없지만 thinking을 지원하면 켬/끔만 있다.
        let coding = by_value("kimi-code/kimi-for-coding");
        assert_eq!(
            coding.efforts(),
            [ReasoningEffort::On, ReasoningEffort::Off]
        );
        assert_eq!(coding.default_effort(), None);

        // thinking 자체가 없으면 선택지가 없다.
        assert!(by_value("zzz-no-thinking/plain").efforts().is_empty());
    }

    #[test]
    fn kimi_catalog_survives_one_entry_with_malformed_support_efforts() {
        // 결함 1 회귀: `support_efforts`가 배열이 아닌 항목 하나 때문에 `[models.*]`
        // 테이블 전체가 사라지면 안 된다. 고치기 전에는 `toml::from_str::<KimiConfig>`
        // 자체가 실패해 `parse_kimi`가 빈 벡터를 돌려줬다.
        let toml = r#"
[models."broken/model"]
display_name = "Broken"
support_efforts = "not-an-array"

[models."ok/model"]
display_name = "Ok"
"#;
        assert_eq!(values(&parse_kimi(toml)), ["ok/model"]);
    }

    #[test]
    fn kimi_catalog_truncation_keeps_default_model_alias() {
        // 결함 2 회귀: `[models.*]`는 BTreeMap이라 별칭 알파벳 순으로 나온다.
        // `kimi-code/...`보다 알파벳상 앞서는 별칭이 상한(64개)을 넘게 쌓이면,
        // 그냥 잘라내는 것만으로는 파일의 `default_model`이 가리키는 실제 기본 모델이
        // 잘려나간다.
        let mut toml = String::from("default_model = \"kimi-code/kimi-for-coding\"\n\n");
        for index in 0..(CATALOG_MODELS_MAX + 10) {
            toml.push_str(&format!("[models.\"aaa-provider/model-{index:04}\"]\n"));
        }
        toml.push_str("[models.\"kimi-code/kimi-for-coding\"]\ndisplay_name = \"K2.7 Coding\"\n");

        let models = parse_kimi(&toml);
        assert_eq!(models.len(), CATALOG_MODELS_MAX);
        assert!(
            models
                .iter()
                .any(|model| model.value() == "kimi-code/kimi-for-coding"),
            "default_model alias must survive truncation"
        );
    }

    #[test]
    fn grok_catalog_maps_efforts_default_flag_fallback_and_unsupported_models() {
        let models = parse_grok(GROK_FIXTURE, None);
        // `priority` 필드가 없으므로 카탈로그 배열 순서를 그대로 유지한다.
        assert_eq!(
            values(&models),
            ["grok-4.5", "grok-4-fast", "grok-3", "grok-3-mini"]
        );

        let by_value = |value: &str| {
            models
                .iter()
                .find(|model| model.value() == value)
                .unwrap_or_else(|| panic!("{value} missing"))
        };

        // `"default": true`로 표시된 항목이 기본 강도가 된다.
        let grok_4_5 = by_value("grok-4.5");
        assert_eq!(grok_4_5.label(), "Grok 4.5");
        assert_eq!(
            grok_4_5.efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ]
        );
        assert_eq!(grok_4_5.default_effort(), Some(ReasoningEffort::High));

        // 어느 항목도 default로 표시되지 않으면 최상위 `reasoning_effort`로 폴백하고,
        // 알 수 없는 강도("minimal")는 조용히 버려진다.
        let grok_4_fast = by_value("grok-4-fast");
        assert_eq!(grok_4_fast.efforts(), [ReasoningEffort::Medium]);
        assert_eq!(grok_4_fast.default_effort(), Some(ReasoningEffort::Medium));

        // `supports_reasoning_effort`가 false면 `reasoning_efforts`가 있어도 선택지가 없다.
        let grok_3 = by_value("grok-3");
        assert!(grok_3.efforts().is_empty());
        assert_eq!(grok_3.default_effort(), None);

        // 필드가 전부 없는 최소 모델도 id를 표시 이름으로 써서 살아남는다.
        let grok_3_mini = by_value("grok-3-mini");
        assert_eq!(grok_3_mini.label(), "grok-3-mini");
        assert!(grok_3_mini.efforts().is_empty());
    }

    #[test]
    fn grok_object_catalog_uses_public_info_key_fallback_and_model_efforts() {
        let models = parse_grok(GROK_OBJECT_FIXTURE, Some("grok-4.6"));
        assert_eq!(values(&models), ["grok-4.6", "grok-4.5"]);
        let grok_46 = models
            .iter()
            .find(|model| model.value() == "grok-4.6")
            .unwrap();
        assert_eq!(grok_46.label(), "Grok 4.6");
        assert_eq!(
            grok_46.efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ]
        );
        assert_eq!(grok_46.default_effort(), Some(ReasoningEffort::High));
        let grok_45 = models
            .iter()
            .find(|model| model.value() == "grok-4.5")
            .unwrap();
        assert_eq!(
            grok_45.efforts(),
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
        assert!(
            models
                .iter()
                .all(|model| !model.label().contains("must-not-be-projected"))
        );
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
    fn grok_object_catalog_keeps_configured_default_beyond_the_cap() {
        let mut json = String::from(r#"{"models":{"#);
        for index in 0..(CATALOG_MODELS_MAX + 10) {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                r#""m{index:03}":{{"info":{{"id":"m{index:03}","name":"M {index}"}}}}"#
            ));
        }
        json.push_str(r#","zzz-configured":{"info":{"name":"Configured"}}"#);
        json.push_str("}}");

        let models = parse_grok(&json, Some("zzz-configured"));
        assert_eq!(models.len(), CATALOG_MODELS_MAX);
        assert_eq!(models[0].value(), "zzz-configured");
        assert_eq!(models[1].value(), "m000");
        assert_eq!(models.last().unwrap().value(), "m062");
        assert!(!models.iter().any(|model| model.value() == "m063"));
    }

    #[test]
    fn grok_object_catalog_does_not_drop_mismatched_configured_key() {
        let models = parse_grok(
            r#"{"models":{
              "configured-key":{"info":{"id":"actual-id","name":"Actual"}},
              "other":{"info":{"id":"other","name":"Other"}}
            }}"#,
            Some("configured-key"),
        );
        assert_eq!(values(&models), ["actual-id", "other"]);
    }

    #[test]
    fn grok_object_catalog_drops_only_entries_missing_info() {
        let models = parse_grok(
            r#"{"models":{
              "missing-info":{"api_key":"must-not-be-projected"},
              "grok-4.6":{"api_key":"must-not-be-projected","info":{"name":"Grok 4.6"}}
            }}"#,
            None,
        );
        assert_eq!(values(&models), ["grok-4.6"]);
        assert_eq!(models[0].label(), "Grok 4.6");
    }

    #[test]
    fn grok_object_catalog_drops_only_non_object_entries() {
        let models = parse_grok(
            r#"{"models":{
              "broken":"not-an-object",
              "grok-4.6":{"api_key":"must-not-be-projected","info":{"name":"Grok 4.6"}}
            }}"#,
            None,
        );
        assert_eq!(values(&models), ["grok-4.6"]);
        assert_eq!(models[0].label(), "Grok 4.6");
    }

    #[test]
    fn qwen_catalog_merges_provider_arrays_deduplicated_and_ordered() {
        let models = parse_qwen(QWEN_FIXTURE);
        // 두 provider 배열을 openai, anthropic 순으로 훑되 겹치는 id는 먼저 나온 쪽이 이긴다.
        assert_eq!(values(&models), ["gpt-4o", "shared-id", "claude-sonnet"]);
        let shared = models
            .iter()
            .find(|model| model.value() == "shared-id")
            .unwrap();
        assert_eq!(shared.label(), "OpenAI Shared");
        // Qwen은 reasoning-effort 플래그가 없으므로 모든 모델이 빈 강도 목록이다.
        assert!(models.iter().all(|model| model.efforts().is_empty()));
        assert!(models.iter().all(|model| model.default_effort().is_none()));
    }

    #[test]
    fn qwen_catalog_dedupes_by_id_and_base_url_pair_with_distinguishable_labels() {
        // 결함 3 회귀: 진짜 키는 `id` 단독이 아니라 `(id, baseUrl)` 쌍이다. 같은 id라도
        // baseUrl이 다르면 별개 모델이고, id와 baseUrl이 모두 같을 때만 진짜 중복이다.
        let json = r#"{
          "modelProviders": {
            "openai": [
              { "id": "gpt-4o", "name": "GPT-4o", "baseUrl": "https://api.openai.com/v1" },
              { "id": "gpt-4o", "name": "GPT-4o via Proxy", "baseUrl": "https://proxy.example.com/v1" },
              { "id": "gpt-4o", "name": "GPT-4o Again", "baseUrl": "https://api.openai.com/v1" }
            ]
          }
        }"#;
        let models = parse_qwen(json);
        // 세 번째 항목만 (id, baseUrl)이 첫 항목과 완전히 같아 진짜 중복으로 걸러진다.
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|model| model.value() == "gpt-4o"));
        // 같은 id가 둘 남으므로 baseUrl의 호스트를 라벨에 덧붙여 구분한다.
        assert_eq!(models[0].label(), "GPT-4o (api.openai.com)");
        assert_eq!(models[1].label(), "GPT-4o via Proxy (proxy.example.com)");
    }

    #[test]
    fn qwen_catalog_includes_custom_provider_keys() {
        // 결함 4 회귀: `modelProviders`는 고정된 다섯 개 authType 밖에도 사용자가 등록한
        // 커스텀 provider id(예: "idealab")를 허용하므로, 그 안의 모델도 보여야 한다.
        let json = r#"{
          "modelProviders": {
            "idealab": [
              { "id": "custom-model", "name": "Custom Model" }
            ]
          }
        }"#;
        let models = parse_qwen(json);
        assert_eq!(values(&models), ["custom-model"]);
        assert_eq!(models[0].label(), "Custom Model");
    }

    #[test]
    fn malformed_or_absent_catalogs_yield_nothing_instead_of_failing() {
        assert!(parse_codex("").is_empty());
        assert!(parse_codex(r#"{"models": "not-an-array"}"#).is_empty());
        assert!(parse_kimi("this is not = valid toml [[[").is_empty());
        assert!(parse_kimi("").is_empty());
        assert!(parse_grok("", None).is_empty());
        assert!(parse_grok(r#"{"models": "not-an-array-or-object"}"#, None).is_empty());
        assert!(parse_qwen("").is_empty());
        assert!(parse_qwen(r#"{"modelProviders": "not-an-object"}"#).is_empty());
        // Qwen은 사용자가 설정을 한 번도 바꾸지 않으면 파일 자체가 없다 — 빈 목록으로 폴백한다.
        assert!(parse_qwen(r#"{"model": {"name": "qwen3-coder-plus"}}"#).is_empty());
        assert_eq!(load(AgentKind::Codex, None, None), CatalogLoad::Unavailable);
        assert_eq!(
            load(AgentKind::Claude, Some(Path::new("/")), None),
            CatalogLoad::Unsupported
        );
        for kind in [AgentKind::Codex, AgentKind::Grok, AgentKind::QwenCode] {
            assert_eq!(
                load(kind, Some(Path::new("/deppy-nonexistent-home")), None),
                CatalogLoad::Missing
            );
        }
    }

    #[test]
    fn catalog_entries_that_the_launch_contract_would_reject_are_dropped() {
        let oversized = "x".repeat(512);
        let json = format!(
            r#"{{"models":[
                {{"slug":"{oversized}","visibility":"list","supported_reasoning_levels":[]}},
                {{"slug":"has\ttab","visibility":"list","supported_reasoning_levels":[]}},
                {{"slug":"  ","visibility":"list","supported_reasoning_levels":[]}},
                {{"slug":"ok","visibility":"list","supported_reasoning_levels":[]}}
            ]}}"#
        );
        assert_eq!(values(&parse_codex(&json)), ["ok"]);
    }

    #[test]
    fn a_default_effort_outside_the_supported_list_is_not_offered() {
        let json = r#"{"models":[{"slug":"m","visibility":"list",
            "default_reasoning_level":"ultra",
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}"#;
        let models = parse_codex(json);
        assert_eq!(models[0].default_effort(), None);
    }

    #[test]
    fn catalog_entry_count_is_bounded() {
        let entries = (0..CATALOG_MODELS_MAX + 20)
            .map(|index| {
                format!(
                    r#"{{"slug":"m{index}","visibility":"list","priority":{index},"supported_reasoning_levels":[]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_codex(&format!(r#"{{"models":[{entries}]}}"#)).len(),
            CATALOG_MODELS_MAX
        );

        let kimi = (0..CATALOG_MODELS_MAX + 20)
            .map(|index| format!("[models.\"m{index:04}\"]\ndisplay_name = \"M{index}\"\n"))
            .collect::<String>();
        assert_eq!(parse_kimi(&kimi).len(), CATALOG_MODELS_MAX);

        let grok_entries = (0..CATALOG_MODELS_MAX + 20)
            .map(|index| format!(r#"{{"id":"m{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_grok(&format!(r#"{{"models":[{grok_entries}]}}"#), None).len(),
            CATALOG_MODELS_MAX
        );

        let qwen_entries = (0..CATALOG_MODELS_MAX + 20)
            .map(|index| format!(r#"{{"id":"m{index}","name":"M{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_qwen(&format!(
                r#"{{"modelProviders":{{"openai":[{qwen_entries}]}}}}"#
            ))
            .len(),
            CATALOG_MODELS_MAX
        );
    }

    #[test]
    fn codex_configured_default_model_reads_top_level_model_key() {
        let toml = r#"
model = "gpt-5.3-codex-spark"

[projects."/Users/jr/Desktop/projects/deppy-sijo"]
trust_level = "trusted"
"#;
        assert_eq!(
            parse_codex_default_model(toml).as_deref(),
            Some("gpt-5.3-codex-spark")
        );
    }

    #[test]
    fn kimi_configured_default_model_reads_top_level_default_model_key() {
        let toml = r#"
default_model = "kimi-code/kimi-for-coding"

[models."kimi-code/kimi-for-coding"]
display_name = "K2.7 Coding"
"#;
        assert_eq!(
            parse_kimi_default_model(toml).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
    }

    #[test]
    fn claude_configured_default_model_reads_top_level_model_key() {
        let json = r#"{"model": "sonnet", "otherKey": true}"#;
        assert_eq!(parse_claude_default_model(json).as_deref(), Some("sonnet"));
    }

    #[test]
    fn grok_configured_default_model_reads_nested_models_table_not_top_level_key() {
        let nested = "[models]\ndefault = \"grok-4.5\"\n";
        assert_eq!(
            parse_grok_default_model(nested).as_deref(),
            Some("grok-4.5")
        );

        // Codex와 달리 최상위 `default` 키는 엉뚱한 자리이므로 무시해야 한다.
        let wrong_place = "default = \"grok-4.5\"\n";
        assert!(parse_grok_default_model(wrong_place).is_none());
    }

    #[test]
    fn grok_configured_defaults_read_only_the_nested_models_table() {
        assert_eq!(
            parse_grok_defaults(
                "default = \"wrong\"\ndefault_reasoning_effort = \"xhigh\"\n\
                 [models]\ndefault = \"  grok-4.6  \"\n\
                 default_reasoning_effort = \" medium \"\n"
            ),
            (Some("grok-4.6".to_owned()), Some(ReasoningEffort::Medium),)
        );
        assert_eq!(
            parse_grok_defaults(
                "[models]\ndefault = \"grok-4.6\"\ndefault_reasoning_effort = \"minimal\"\n"
            ),
            (Some("grok-4.6".to_owned()), None)
        );
    }

    #[test]
    fn qwen_configured_default_model_reads_nested_model_name_key() {
        let json = r#"{"model": {"name": "qwen3-coder-plus"}}"#;
        assert_eq!(
            parse_qwen_default_model(json).as_deref(),
            Some("qwen3-coder-plus")
        );
        assert!(parse_qwen_default_model(r#"{"otherKey": true}"#).is_none());
    }

    #[test]
    fn configured_default_model_trims_and_rejects_blank_values() {
        assert_eq!(
            parse_codex_default_model(r#"model = "  gpt-5.3-codex-spark  ""#).as_deref(),
            Some("gpt-5.3-codex-spark")
        );
        assert!(parse_codex_default_model(r#"model = "   ""#).is_none());
        assert!(parse_claude_default_model(r#"{"model": ""}"#).is_none());
    }

    #[test]
    fn configured_default_model_is_none_for_missing_malformed_or_absent_key() {
        // 키가 아예 없는 빈 파일: 파싱은 되지만 값이 없다.
        assert!(parse_codex_default_model("").is_none());
        // 타입이 문자열이 아니면 파싱 자체가 실패한다.
        assert!(parse_codex_default_model("model = 42").is_none());
        assert!(parse_kimi_default_model("this is not = valid toml [[[").is_none());
        assert!(parse_claude_default_model("not json").is_none());
        assert!(parse_claude_default_model(r#"{"other": "x"}"#).is_none());
        // home이 없거나 홈 아래 파일 자체가 없는 경우.
        assert!(configured_default_model(AgentKind::Codex, None).is_none());
        assert!(
            configured_default_model(AgentKind::Codex, Some(Path::new("/deppy-nonexistent-home")))
                .is_none()
        );
    }

    #[test]
    fn non_model_supporting_kind_yields_none() {
        assert!(configured_default_model(AgentKind::Gemini, Some(Path::new("/"))).is_none());
    }

    #[test]
    fn kimi_env_var_guard_only_suppresses_when_actually_non_empty() {
        let dir = unique_temp_dir("kimi-default-model-env");
        let _ = std::fs::create_dir_all(dir.join(".kimi-code"));
        std::fs::write(
            dir.join(".kimi-code/config.toml"),
            r#"default_model = "kimi-code/kimi-for-coding""#,
        )
        .unwrap();

        // KIMI_MODEL_NAME이 실제로 설정돼 있으면 합성 모델이라 안다고 주장하지 않는다.
        assert!(kimi_configured_default_model(dir.as_path(), Some("runtime-model")).is_none());
        // 설정은 됐지만 공백뿐이면 "설정 안 됨"과 동일하게 취급해 파일을 읽는다.
        assert_eq!(
            kimi_configured_default_model(dir.as_path(), Some("   ")).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        // 아예 설정되지 않은 경우도 파일을 읽는다.
        assert_eq!(
            kimi_configured_default_model(dir.as_path(), None).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_default_model_reads_codex_and_claude_config_from_disk() {
        let dir = unique_temp_dir("configured-default-model");
        let _ = std::fs::create_dir_all(dir.join(".codex"));
        let _ = std::fs::create_dir_all(dir.join(".claude"));
        std::fs::write(
            dir.join(".codex/config.toml"),
            "model = \"gpt-5.3-codex-spark\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"model": "sonnet", "effortLevel": "high"}"#,
        )
        .unwrap();

        assert_eq!(
            configured_default_model(AgentKind::Codex, Some(dir.as_path())).as_deref(),
            Some("gpt-5.3-codex-spark")
        );
        assert_eq!(
            configured_default_model(AgentKind::Claude, Some(dir.as_path())).as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            claude_configured_defaults(Some(dir.as_path())),
            (Some("sonnet".to_owned()), Some("high".to_owned()))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_catalog_files_are_not_parsed() {
        let dir = unique_temp_dir("bound");
        let _ = std::fs::create_dir_all(dir.join(".codex"));
        let path = dir.join(".codex/models_cache.json");
        let padding = " ".repeat(usize::try_from(CATALOG_MAX_BYTES).unwrap_or(usize::MAX) + 1);
        std::fs::write(&path, format!("{padding}{{\"models\":[]}}")).unwrap();
        assert!(read_bounded(&path).is_none());
        assert_eq!(
            load(AgentKind::Codex, Some(dir.as_path()), None),
            CatalogLoad::Unavailable
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
