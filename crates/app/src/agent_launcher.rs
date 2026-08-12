use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const EXECUTABLE_PATH_MAX_BYTES: usize = 4 * 1024;
const MODEL_MAX_BYTES: usize = 256;
/// 표시 이름 상한. 카탈로그가 긴 문자열을 넣어도 콤보박스가 무너지지 않게 한다.
const MODEL_LABEL_MAX_BYTES: usize = 128;
/// 에이전트 하나가 제시할 모델 수 상한. 카탈로그 상한과 같은 값을 유지한다.
const MODELS_PER_AGENT_MAX: usize = 64;
const DETECTION_PATH_ITEMS_MAX: usize = 96;
const DETECTION_LAUNCH_PATH_MAX_BYTES: usize = 32 * 1024;
const CODEX_EFFORTS_XHIGH: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const CODEX_EFFORTS_MAX: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];
const CODEX_EFFORTS_ULTRA: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
    ReasoningEffort::Ultra,
];
const CLAUDE_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];
// Kimi 모델 카탈로그가 선언하는 `support_efforts` 그대로다(low/high/max).
const KIMI_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::High,
    ReasoningEffort::Max,
];
// `support_efforts`가 없는 Kimi 모델은 강도 단계 없이 thinking 켬/끔만 있다.
const KIMI_THINKING_TOGGLE: &[ReasoningEffort] = &[ReasoningEffort::On, ReasoningEffort::Off];
// grok-4.5가 광고하는 단계. Grok의 전체 어휘는 none/minimal도 포함하지만 모델마다
// 부분집합만 받으므로, 카탈로그를 못 읽을 때 쓰는 이 폴백은 기본 모델 기준으로 둔다.
const GROK_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];

#[cfg(unix)]
const AGENT_THEN_SHELL_SCRIPT: &str = r#""$@"; stty sane 2>/dev/null || true; unset DEPPY_AGENT_EXECUTABLE DEPPY_SHIM_GUARD; exec "${SHELL:-/bin/sh}""#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AgentKind {
    Claude,
    Codex,
    OpenCode,
    Gemini,
    Aider,
    Goose,
    Amp,
    Kimi,
    QwenCode,
    Grok,
    Cursor,
    Copilot,
}

/// 앱에 컴파일해 넣은 모델 항목. 디스크 카탈로그를 못 읽을 때의 폴백이다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BuiltinModel {
    value: &'static str,
    label: &'static str,
    efforts: &'static [ReasoningEffort],
    default_effort: Option<ReasoningEffort>,
}

/// 런처가 제시하는 모델 하나. 디스크 카탈로그에서 읽을 수도 있으므로 소유 데이터다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    value: String,
    label: String,
    efforts: Vec<ReasoningEffort>,
    default_effort: Option<ReasoningEffort>,
}

impl ModelChoice {
    /// 실행 계약이 거부할 값은 애초에 목록에 올리지 않는다. 표시 이름이 비었거나
    /// 지나치게 길면 식별자를 그대로 쓰고, 선언된 기본 강도가 지원 목록 밖이면 버린다.
    pub(crate) fn new(
        value: &str,
        label: &str,
        efforts: Vec<ReasoningEffort>,
        default_effort: Option<ReasoningEffort>,
    ) -> Option<Self> {
        let value = value.trim();
        if value.is_empty()
            || value.len() > MODEL_MAX_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return None;
        }
        let label = label.trim();
        let label = if label.is_empty() || label.len() > MODEL_LABEL_MAX_BYTES {
            value.to_owned()
        } else {
            label.replace(|c: char| c.is_control(), " ")
        };
        Some(Self {
            value: value.to_owned(),
            label,
            default_effort: default_effort.filter(|effort| efforts.contains(effort)),
            efforts,
        })
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn efforts(&self) -> &[ReasoningEffort] {
        &self.efforts
    }

    pub(crate) fn default_effort(&self) -> Option<ReasoningEffort> {
        self.default_effort
    }
}

impl AgentKind {
    pub(crate) const ALL: [Self; 12] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Gemini,
        Self::Aider,
        Self::Goose,
        Self::Amp,
        Self::Kimi,
        Self::QwenCode,
        Self::Grok,
        Self::Cursor,
        Self::Copilot,
    ];

    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Aider => "aider",
            Self::Goose => "goose",
            Self::Amp => "amp",
            Self::Kimi => "kimi",
            Self::QwenCode => "qwen-code",
            Self::Grok => "grok",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
        }
    }

    pub(crate) const fn stable_config_id(self) -> &'static str {
        match self {
            Self::Claude => "deppy-builtin-claude",
            Self::Codex => "deppy-builtin-codex",
            Self::OpenCode => "deppy-builtin-opencode",
            Self::Gemini => "deppy-builtin-gemini",
            Self::Aider => "deppy-builtin-aider",
            Self::Goose => "deppy-builtin-goose",
            Self::Amp => "deppy-builtin-amp",
            Self::Kimi => "deppy-builtin-kimi",
            Self::QwenCode => "deppy-builtin-qwen-code",
            Self::Grok => "deppy-builtin-grok",
            Self::Cursor => "deppy-builtin-cursor",
            Self::Copilot => "deppy-builtin-copilot",
        }
    }

    pub(crate) fn from_stable_config_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.stable_config_id() == id)
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex (ChatGPT)",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini CLI",
            Self::Aider => "Aider",
            Self::Goose => "Goose",
            Self::Amp => "Amp",
            Self::Kimi => "Kimi CLI",
            Self::QwenCode => "Qwen Code",
            Self::Grok => "Grok",
            Self::Cursor => "Cursor Agent",
            Self::Copilot => "GitHub Copilot",
        }
    }

    pub(crate) const fn badge(self) -> &'static str {
        match self {
            Self::Claude => "CL",
            Self::Codex => "CX",
            Self::OpenCode => "OC",
            Self::Gemini => "GM",
            Self::Aider => "AI",
            Self::Goose => "GO",
            Self::Amp => "AM",
            Self::Kimi => "KI",
            Self::QwenCode => "QW",
            Self::Grok => "GK",
            Self::Cursor => "CU",
            Self::Copilot => "CP",
        }
    }

    pub(crate) const fn badge_color(self) -> (u8, u8, u8) {
        match self {
            Self::Claude => (0xd9, 0x77, 0x57),
            Self::Codex => (0x10, 0xa3, 0x7f),
            Self::OpenCode => (0x68, 0x7a, 0xf2),
            Self::Gemini => (0x62, 0x8f, 0xe8),
            Self::Aider => (0xa4, 0x69, 0xd8),
            Self::Goose => (0xf0, 0xa4, 0x3c),
            Self::Amp => (0xf0, 0x62, 0x92),
            Self::Kimi => (0x4e, 0x7d, 0xf2),
            Self::QwenCode => (0x72, 0x63, 0xd9),
            Self::Grok => (0x1d, 0x1d, 0x1f),
            Self::Cursor => (0x55, 0x58, 0x60),
            Self::Copilot => (0x78, 0x7f, 0x89),
        }
    }

    const fn detect_command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Gemini => "gemini",
            Self::Aider => "aider",
            Self::Goose => "goose",
            Self::Amp => "amp",
            Self::Kimi => "kimi",
            Self::QwenCode => "qwen",
            Self::Grok => "grok",
            Self::Cursor => "cursor-agent",
            Self::Copilot => "copilot",
        }
    }

    pub(crate) const fn supports_model(self) -> bool {
        matches!(
            self,
            Self::Claude | Self::Codex | Self::Kimi | Self::Grok | Self::QwenCode
        )
    }

    const fn builtin_models(self) -> &'static [BuiltinModel] {
        match self {
            Self::Claude => &[
                BuiltinModel {
                    value: "sonnet",
                    label: "Sonnet",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: Some(ReasoningEffort::High),
                },
                BuiltinModel {
                    value: "opus",
                    label: "Opus",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: Some(ReasoningEffort::High),
                },
                BuiltinModel {
                    value: "fable",
                    label: "Fable",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: Some(ReasoningEffort::High),
                },
            ],
            Self::Codex => &[
                BuiltinModel {
                    value: "gpt-5.6-sol",
                    label: "GPT-5.6-Sol",
                    efforts: CODEX_EFFORTS_ULTRA,
                    default_effort: Some(ReasoningEffort::Low),
                },
                BuiltinModel {
                    value: "gpt-5.6-terra",
                    label: "GPT-5.6-Terra",
                    efforts: CODEX_EFFORTS_ULTRA,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                BuiltinModel {
                    value: "gpt-5.6-luna",
                    label: "GPT-5.6-Luna",
                    efforts: CODEX_EFFORTS_MAX,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                BuiltinModel {
                    value: "gpt-5.5",
                    label: "GPT-5.5",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                BuiltinModel {
                    value: "gpt-5.4",
                    label: "GPT-5.4",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                BuiltinModel {
                    value: "gpt-5.4-mini",
                    label: "GPT-5.4-Mini",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                BuiltinModel {
                    value: "gpt-5.3-codex-spark",
                    label: "GPT-5.3-Codex-Spark",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::High),
                },
            ],
            Self::Kimi => &[
                BuiltinModel {
                    value: "kimi-code/kimi-for-coding",
                    label: "K2.7 Coding",
                    efforts: KIMI_THINKING_TOGGLE,
                    default_effort: None,
                },
                BuiltinModel {
                    value: "kimi-code/kimi-for-coding-highspeed",
                    label: "K2.7 Coding Highspeed",
                    efforts: KIMI_THINKING_TOGGLE,
                    default_effort: None,
                },
                BuiltinModel {
                    value: "kimi-code/k3",
                    label: "K3",
                    efforts: KIMI_EFFORTS,
                    default_effort: Some(ReasoningEffort::High),
                },
                BuiltinModel {
                    value: "kimi-code/k3-256k",
                    label: "K3-256k",
                    efforts: KIMI_EFFORTS,
                    default_effort: Some(ReasoningEffort::High),
                },
            ],
            // Grok의 `~/.grok/models_cache.json`은 로그인 후 서버에서 받아야 생긴다.
            // 그 전까지는 내장 기본 모델 하나만 제시한다.
            Self::Grok => &[BuiltinModel {
                value: "grok-4.5",
                label: "Grok 4.5",
                efforts: GROK_EFFORTS,
                default_effort: Some(ReasoningEffort::High),
            }],
            // Qwen Code는 모델 카탈로그를 받아오지 않는다. OAuth 기본 모델만 확실하고,
            // 나머지는 사용자가 settings.json에 직접 선언해야 쓸 수 있어 카탈로그에서 읽는다.
            // 추론 강도는 CLI 플래그가 없어(설정 파일/슬래시 명령 전용) 제시하지 않는다.
            Self::QwenCode => &[BuiltinModel {
                value: "coder-model",
                label: "Qwen Coder",
                efforts: &[],
                default_effort: None,
            }],
            _ => &[],
        }
    }

    pub(crate) const fn supports_yolo(self) -> bool {
        !matches!(self, Self::OpenCode)
    }

    pub(crate) const fn supports_deppy_shim(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    /// 카탈로그에 없는 모델(=CLI 자신의 기본 모델)에 쓰는 보수적 강도 목록.
    ///
    /// Kimi는 여기에 넣지 않는다. Kimi 모델은 단계형(`support_efforts`)과 boolean
    /// thinking(켬/끔) 두 종류인데 모르는 모델이 어느 쪽인지 알 수 없다. 틀린 종류의
    /// 조작을 제시하느니 제시하지 않는 편이 낫다.
    const fn fallback_efforts(self) -> &'static [ReasoningEffort] {
        match self {
            Self::Codex => CODEX_EFFORTS_XHIGH,
            Self::Claude => CLAUDE_EFFORTS,
            Self::Grok => GROK_EFFORTS,
            _ => &[],
        }
    }

    /// 카탈로그에 없는 모델의 기본 강도. 이 값이 없으면 UI가 목록 첫 단계(=가장 낮음)를
    /// 고르게 되어, CLI를 그냥 실행했을 때보다 낮은 강도로 조용히 실행된다.
    /// 두 값 모두 해당 CLI가 자기 모델들에 선언한 기본값에서 가져왔다.
    const fn fallback_default_effort(self) -> Option<ReasoningEffort> {
        match self {
            // Claude 카탈로그의 5계열 모델은 전부 `default_effort: "high"`다.
            Self::Claude => Some(ReasoningEffort::High),
            // Codex 카탈로그 7개 중 5개가 `medium`이다.
            Self::Codex => Some(ReasoningEffort::Medium),
            // grok-4.5가 선언하는 기본값이다.
            Self::Grok => Some(ReasoningEffort::High),
            _ => None,
        }
    }

    /// 내장 목록의 모델 값만. PTY 모델 순환(`pty_effort`)이 자기 사다리를 이것과
    /// 대조해 두 카탈로그가 말없이 갈라지지 않게 한다 — 대조 전용이라 테스트에서만 쓴다.
    #[cfg(test)]
    pub(crate) fn builtin_model_values(self) -> Vec<&'static str> {
        self.builtin_models()
            .iter()
            .map(|model| model.value)
            .collect()
    }

    /// 디스크 카탈로그를 못 읽었을 때 쓰는 내장 목록.
    fn builtin_model_choices(self) -> Vec<ModelChoice> {
        self.builtin_models()
            .iter()
            .filter_map(|model| {
                ModelChoice::new(
                    model.value,
                    model.label,
                    model.efforts.to_vec(),
                    model.default_effort,
                )
            })
            .collect()
    }
}

pub(crate) fn is_builtin_config_id(id: &str) -> bool {
    AgentKind::from_stable_config_id(id).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    // `support_efforts`를 선언하지 않은 Kimi 모델의 boolean thinking 값이다.
    // Kimi 자신도 같은 effort 필드에 이 두 의사(pseudo) 값을 쓴다.
    On,
    Off,
}

impl ReasoningEffort {
    pub(crate) const fn value(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// 강도 단계가 아니라 thinking 켬/끔만 고르는 목록인지.
    pub(crate) fn is_thinking_toggle(levels: &[Self]) -> bool {
        !levels.is_empty()
            && levels
                .iter()
                .all(|level| matches!(level, Self::On | Self::Off))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DetectedAgent {
    kind: AgentKind,
    executable: PathBuf,
    launch_path: Option<Arc<str>>,
    /// 감지 시점에 확정된 모델 목록. 디스크 카탈로그를 읽었으면 그 결과, 아니면 내장 목록.
    models: Vec<ModelChoice>,
    /// CLI가 자기 설정에 적어 둔 기본 모델(목록 안에 있을 때만). 런처는 이 값을 미리
    /// 골라 두어, 앱으로 띄운 결과가 CLI를 그냥 실행한 것과 같게 유지한다.
    default_model: Option<String>,
}

impl DetectedAgent {
    pub(crate) fn kind(&self) -> AgentKind {
        self.kind
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }

    pub(crate) fn models(&self) -> &[ModelChoice] {
        &self.models
    }

    /// 런처가 처음 제시할 모델. CLI 설정의 기본 모델을 쓰되, 그 값이 카탈로그에 없으면
    /// (설정이 비었거나 모델이 내려갔거나 env로 대체된 경우) 목록 첫 항목으로 물러난다.
    pub(crate) fn initial_model(&self) -> &str {
        self.default_model
            .as_deref()
            .filter(|model| find_model(&self.models, model).is_some())
            .or_else(|| self.models.first().map(ModelChoice::value))
            .unwrap_or_default()
    }

    pub(crate) fn supported_efforts(&self, model: &str) -> &[ReasoningEffort] {
        efforts_for(self.kind, &self.models, model)
    }
}

/// 목록에서 이 식별자의 모델을 찾는다. 빈 문자열(= CLI 자신의 기본 모델)은 못 찾는다.
pub(crate) fn find_model<'a>(models: &'a [ModelChoice], model: &str) -> Option<&'a ModelChoice> {
    models.iter().find(|choice| choice.value() == model)
}

/// 실행 계약이 허용하는 강도 목록. 카탈로그에 없는 모델(= CLI 자신의 기본 모델)에는
/// 종류별 보수적 폴백을 쓴다. UI는 이 폴백을 제시하지 않고, 여기서는 UI 밖에서 들어온
/// 요청을 막는 상한으로만 쓴다.
fn efforts_for<'a>(
    kind: AgentKind,
    models: &'a [ModelChoice],
    model: &str,
) -> &'a [ReasoningEffort] {
    find_model(models, model).map_or_else(|| kind.fallback_efforts(), ModelChoice::efforts)
}

impl std::fmt::Debug for DetectedAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DetectedAgent")
            .field("kind", &self.kind)
            .field("executable", &"[REDACTED]")
            .field(
                "launch_path",
                &self.launch_path.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DetectionSnapshot {
    agents: Vec<DetectedAgent>,
    claude_default_model: Option<String>,
    claude_default_effort: Option<String>,
}

impl DetectionSnapshot {
    pub(crate) fn agents(&self) -> &[DetectedAgent] {
        &self.agents
    }

    pub(crate) fn find(&self, kind: AgentKind) -> Option<&DetectedAgent> {
        self.agents.iter().find(|agent| agent.kind == kind)
    }

    pub(crate) fn claude_defaults(&self) -> (Option<&str>, Option<&str>) {
        (
            self.claude_default_model.as_deref(),
            self.claude_default_effort.as_deref(),
        )
    }

    #[cfg(test)]
    pub(crate) fn from_test_agents(agents: impl IntoIterator<Item = (AgentKind, PathBuf)>) -> Self {
        Self {
            agents: agents
                .into_iter()
                .map(|(kind, executable)| DetectedAgent {
                    kind,
                    executable,
                    launch_path: None,
                    models: kind.builtin_model_choices(),
                    default_model: None,
                })
                .collect(),
            claude_default_model: None,
            claude_default_effort: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchOptions {
    pub(crate) model: String,
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) yolo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchSpecErrorCode {
    InvalidExecutable,
    InvalidModel,
    UnsupportedModel,
    UnsupportedEffort,
    UnsupportedYolo,
}

pub(crate) struct LaunchSpec {
    kind: AgentKind,
    command: String,
    args: Vec<String>,
    env_plain: Vec<(String, String)>,
}

impl LaunchSpec {
    pub(crate) fn into_parts(self) -> (AgentKind, String, Vec<String>, Vec<(String, String)>) {
        (self.kind, self.command, self.args, self.env_plain)
    }
}

#[cfg(unix)]
pub(crate) fn wrap_agent_then_shell(command: String, args: Vec<String>) -> (String, Vec<String>) {
    let mut wrapped_args = Vec::with_capacity(args.len() + 4);
    wrapped_args.push("-c".to_owned());
    wrapped_args.push(AGENT_THEN_SHELL_SCRIPT.to_owned());
    wrapped_args.push("deppy-agent-session".to_owned());
    wrapped_args.push(command);
    wrapped_args.extend(args);
    ("/bin/sh".to_owned(), wrapped_args)
}

#[cfg(not(unix))]
pub(crate) fn wrap_agent_then_shell(command: String, args: Vec<String>) -> (String, Vec<String>) {
    (command, args)
}

/// 설치된 에이전트를 찾고, 각 에이전트의 모델 목록을 그 자리에서 확정한다.
/// 파일 I/O를 하므로 렌더 스레드가 아니라 lazy 감지 워커에서만 호출한다.
pub(crate) fn detect_installed_agents(excluded_directory: Option<&Path>) -> DetectionSnapshot {
    let paths = detection_paths(excluded_directory);
    let launch_path = launch_search_path(&paths);
    let home = crate::paths::home_dir();
    let (claude_default_model, claude_default_effort) =
        crate::agent_model_catalog::claude_configured_defaults(home.as_deref());
    let agents = AgentKind::ALL
        .into_iter()
        .filter_map(|kind| {
            resolve_executable(kind.detect_command(), &paths).map(|executable| {
                // 설정 파일은 종류마다 한 번만 읽는다. 모델 목록과 초기 선택이 같은 값을
                // 쓰므로 각각 읽으면 같은 파일을 두 번 열고 파싱하게 된다.
                let configured = if kind == AgentKind::Claude {
                    claude_default_model.clone()
                } else {
                    crate::agent_model_catalog::configured_default_model(kind, home.as_deref())
                };
                DetectedAgent {
                    kind,
                    executable,
                    launch_path: launch_path.clone(),
                    models: resolve_models(kind, home.as_deref(), configured.as_deref()),
                    default_model: configured,
                }
            })
        })
        .collect();
    DetectionSnapshot {
        agents,
        claude_default_model,
        claude_default_effort,
    }
}

/// CLI가 디스크에 남긴 카탈로그를 우선하고, 없거나 못 읽으면 내장 목록으로 폴백한다.
/// 폴백 덕분에 새 설치·로그아웃·손상된 파일에서도 런처가 빈 목록이 되지 않는다.
fn resolve_models(
    kind: AgentKind,
    home: Option<&Path>,
    configured: Option<&str>,
) -> Vec<ModelChoice> {
    let mut models = if crate::agent_model_catalog::has_disk_catalog(kind) {
        crate::agent_model_catalog::load(kind, home)
    } else {
        Vec::new()
    };
    if models.is_empty() {
        models = kind.builtin_model_choices();
    }
    if let Some(configured) = configured {
        adopt_model(kind, configured, &mut models);
    }
    models
}

/// CLI 설정이 가리키는 모델이 목록에 없으면 그 모델을 목록 맨 앞에 넣는다.
///
/// 카탈로그가 모든 모델을 담지는 못한다 — Claude은 카탈로그 자체가 없어 내장 별칭만 갖고,
/// `opus[1m]` 같은 변형이나 사용자가 직접 추가한 모델은 빠진다. 그 상태로 두면 런처가
/// 사용자가 설정해 둔 모델 대신 목록 첫 항목을 조용히 띄우게 된다. CLI가 설정에 적어 둔
/// 값은 그 CLI에서 유효한 값이므로, 모르는 값이어도 선택지로 인정한다.
fn adopt_model(kind: AgentKind, configured: &str, models: &mut Vec<ModelChoice>) {
    if models.is_empty() {
        return;
    }
    if find_model(models, configured).is_some() {
        return;
    }
    // 강도 목록도 기본 강도도 알 수 없으므로 그 종류의 보수적 폴백을 쓴다. 기본 강도를
    // 비워 두면 UI가 목록 첫 단계(=가장 낮음)를 골라, CLI 단독 실행보다 낮은 강도로
    // 조용히 실행된다.
    let Some(choice) = ModelChoice::new(
        configured,
        configured,
        kind.fallback_efforts().to_vec(),
        kind.fallback_default_effort(),
    ) else {
        return;
    };
    // 상한을 먼저 확보하고 넣는다. 넣고 나서 자르면 목록 끝의 진짜 카탈로그 항목이
    // 밀려나고, 그게 사용자가 고른 모델이면 다음 새로고침에 말없이 바뀐다.
    models.truncate(MODELS_PER_AGENT_MAX.saturating_sub(1));
    models.insert(0, choice);
}

pub(crate) fn build_launch_spec(
    agent: &DetectedAgent,
    options: LaunchOptions,
    shim: Option<&Path>,
) -> Result<LaunchSpec, LaunchSpecErrorCode> {
    let executable = valid_executable_string(agent.executable())?;
    let command = match shim {
        Some(path) => valid_executable_string(path)?,
        None => executable.clone(),
    };
    let model = options.model.trim();
    if model.len() > MODEL_MAX_BYTES || model.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(LaunchSpecErrorCode::InvalidModel);
    }
    if !model.is_empty()
        && (!agent.kind.supports_model()
            || !agent.models().iter().any(|choice| choice.value() == model))
    {
        return Err(LaunchSpecErrorCode::UnsupportedModel);
    }
    if options
        .effort
        .is_some_and(|effort| !agent.supported_efforts(model).contains(&effort))
    {
        return Err(LaunchSpecErrorCode::UnsupportedEffort);
    }
    if options.yolo && !agent.kind.supports_yolo() {
        return Err(LaunchSpecErrorCode::UnsupportedYolo);
    }

    let mut args = Vec::new();
    let mut env_plain = Vec::new();
    if let Some(path) = &agent.launch_path {
        env_plain.push(("PATH".to_owned(), path.to_string()));
    }
    if shim.is_some() {
        env_plain.push(("DEPPY_AGENT_EXECUTABLE".to_owned(), executable));
    }
    if options.yolo {
        append_yolo(agent.kind, &mut args, &mut env_plain);
    }
    if !model.is_empty() {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    if let Some(effort) = options.effort {
        match agent.kind {
            AgentKind::Codex => {
                args.push("--config".to_owned());
                args.push(format!("model_reasoning_effort=\"{}\"", effort.value()));
            }
            AgentKind::Claude => {
                args.push("--effort".to_owned());
                args.push(effort.value().to_owned());
            }
            AgentKind::Grok => {
                args.push("--reasoning-effort".to_owned());
                args.push(effort.value().to_owned());
            }
            // Kimi CLI에는 effort 플래그가 없고 이 env 오버라이드만 있다.
            AgentKind::Kimi => {
                env_plain.push((
                    "KIMI_MODEL_THINKING_EFFORT".to_owned(),
                    effort.value().to_owned(),
                ));
            }
            _ => return Err(LaunchSpecErrorCode::UnsupportedEffort),
        }
    }

    Ok(LaunchSpec {
        kind: agent.kind,
        command,
        args,
        env_plain,
    })
}

fn append_yolo(kind: AgentKind, args: &mut Vec<String>, env_plain: &mut Vec<(String, String)>) {
    match kind {
        AgentKind::Claude => args.push("--dangerously-skip-permissions".to_owned()),
        AgentKind::Codex => {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_owned());
        }
        AgentKind::Gemini => args.push("--yolo".to_owned()),
        AgentKind::Aider => args.push("--yes-always".to_owned()),
        AgentKind::Goose => env_plain.push(("GOOSE_MODE".to_owned(), "auto".to_owned())),
        AgentKind::Amp => args.push("--dangerously-allow-all".to_owned()),
        AgentKind::Kimi => args.push("--yolo".to_owned()),
        // `--yolo`도 파싱되지만 `--help`에 실린 정식 이름은 이쪽이다.
        AgentKind::Grok => args.push("--always-approve".to_owned()),
        AgentKind::QwenCode => {
            args.push("--approval-mode".to_owned());
            args.push("yolo".to_owned());
        }
        AgentKind::Cursor | AgentKind::Copilot => args.push("--yolo".to_owned()),
        AgentKind::OpenCode => {}
    }
}

fn detection_paths(excluded_directory: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    if let Some(value) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&value) {
            push_path_env_entry(&mut paths, &mut seen, path);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if let Some(home) = home {
        for suffix in [
            ".local/bin",
            ".cargo/bin",
            ".kimi-code/bin",
            ".cursor/bin",
            ".asdf/shims",
            ".bun/bin",
            ".npm-global/bin",
            ".volta/bin",
            ".local/share/pnpm",
            ".local/share/mise/shims",
            "Library/pnpm",
        ] {
            push_detection_path(&mut paths, &mut seen, home.join(suffix));
        }
        push_versioned_detection_paths(
            &mut paths,
            &mut seen,
            &home.join(".nvm/versions/node"),
            Path::new("bin"),
        );
        push_versioned_detection_paths(
            &mut paths,
            &mut seen,
            &home.join(".local/share/fnm/node-versions"),
            Path::new("installation/bin"),
        );
        push_versioned_detection_paths(
            &mut paths,
            &mut seen,
            &home.join(".asdf/installs/nodejs"),
            Path::new("bin"),
        );
        push_versioned_detection_paths(
            &mut paths,
            &mut seen,
            &home.join(".local/share/mise/installs/node"),
            Path::new("bin"),
        );
    }
    for path in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        push_detection_path(&mut paths, &mut seen, PathBuf::from(path));
    }
    if let Some(excluded) = excluded_directory {
        exclude_detection_directory(&mut paths, excluded);
    }
    paths
}

fn exclude_detection_directory(paths: &mut Vec<PathBuf>, excluded: &Path) {
    let excluded = std::fs::canonicalize(excluded).unwrap_or_else(|_| excluded.to_path_buf());
    paths.retain(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()) != excluded);
}

fn push_versioned_detection_paths(
    paths: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    root: &Path,
    suffix: &Path,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut versions = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .take(DETECTION_PATH_ITEMS_MAX)
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    for version in versions {
        push_detection_path(paths, seen, version.join(suffix));
    }
}

fn launch_search_path(paths: &[PathBuf]) -> Option<Arc<str>> {
    let joined = std::env::join_paths(paths.iter().filter(|path| path.is_dir())).ok()?;
    let joined = joined.to_str()?;
    (!joined.is_empty() && joined.len() <= DETECTION_LAUNCH_PATH_MAX_BYTES)
        .then(|| Arc::<str>::from(joined))
}

fn push_detection_path(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if paths.len() >= DETECTION_PATH_ITEMS_MAX {
        return;
    }
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() > EXECUTABLE_PATH_MAX_BYTES || bytes.contains(&0) {
        return;
    }
    if seen.insert(path.clone()) {
        paths.push(path);
    }
}

/// 프로세스가 물려받은 `PATH` 항목 전용 게이트. 홈 하위 잘 알려진 위치나 nvm/asdf 버전
/// 스캔처럼 우리가 직접 구성하는 후보와 달리, `PATH`는 사용자 셸이나 다른 도구가 채워
/// 넣은 값이라 신뢰 경계가 다르다 — OS 임시 디렉터리 아래는 걸러낸다.
///
/// 실제로 설치된 CLI가 임시 디렉터리에 살 리는 없다 — 거기 있는 건 다른 도구(cmux 등)가
/// 터미널 세션마다 PATH 맨 앞에 까는 per-invocation hook shim뿐이다. 그런 shim을 "진짜
/// codex/claude"로 오인해 `DEPPY_AGENT_EXECUTABLE`로 넘기면, 우리 shim이 그 위에 우리
/// hook 인자를 얹어 exec하고 그 shim이 다시 자기 hook을 넣어 `codex`에 전달한다 — 결과
/// `--dangerously-bypass-hook-trust` 같은 플래그가 두 번 전달돼 codex가 시작을 거부한다
/// (실측 재현: cmux의 codex wrapper가 동일 패턴으로 hook을 주입한다).
fn push_path_env_entry(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if is_transient_shim_directory(&path) {
        return;
    }
    push_detection_path(paths, seen, path);
}

fn is_transient_shim_directory(path: &Path) -> bool {
    let temp_dir = std::env::temp_dir();
    !temp_dir.as_os_str().is_empty() && path.starts_with(&temp_dir)
}

fn resolve_executable(command: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    for directory in paths {
        for name in executable_names(command) {
            let candidate = directory.join(name);
            if executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn executable_names(command: &str) -> Vec<String> {
    vec![
        format!("{command}.exe"),
        format!("{command}.cmd"),
        format!("{command}.bat"),
        command.to_owned(),
    ]
}

#[cfg(not(windows))]
fn executable_names(command: &str) -> [&str; 1] {
    [command]
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn valid_executable_string(path: &Path) -> Result<String, LaunchSpecErrorCode> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if !path.is_absolute()
        || bytes.is_empty()
        || bytes.len() > EXECUTABLE_PATH_MAX_BYTES
        || bytes.contains(&0)
    {
        return Err(LaunchSpecErrorCode::InvalidExecutable);
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or(LaunchSpecErrorCode::InvalidExecutable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected(kind: AgentKind) -> DetectedAgent {
        DetectedAgent {
            kind,
            executable: PathBuf::from(format!("/opt/deppy/{}", kind.detect_command())),
            launch_path: None,
            models: kind.builtin_model_choices(),
            default_model: None,
        }
    }

    #[test]
    fn codex_launch_combines_explicit_yolo_model_and_xhigh() {
        let spec = build_launch_spec(
            &detected(AgentKind::Codex),
            LaunchOptions {
                model: "gpt-5.4".to_owned(),
                effort: Some(ReasoningEffort::XHigh),
                yolo: true,
            },
            Some(Path::new("/opt/deppy/shims/codex")),
        )
        .unwrap();
        let (_, command, args, env) = spec.into_parts();
        assert_eq!(command, "/opt/deppy/shims/codex");
        assert_eq!(
            args,
            [
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5.4",
                "--config",
                "model_reasoning_effort=\"xhigh\"",
            ]
        );
        assert_eq!(
            env,
            [(
                "DEPPY_AGENT_EXECUTABLE".to_owned(),
                "/opt/deppy/codex".to_owned()
            )]
        );
    }

    #[test]
    fn claude_launch_uses_native_effort_and_yolo_flags() {
        let spec = build_launch_spec(
            &detected(AgentKind::Claude),
            LaunchOptions {
                model: "sonnet".to_owned(),
                effort: Some(ReasoningEffort::Max),
                yolo: true,
            },
            None,
        )
        .unwrap();
        let (_, _, args, env) = spec.into_parts();
        assert_eq!(
            args,
            [
                "--dangerously-skip-permissions",
                "--model",
                "sonnet",
                "--effort",
                "max",
            ]
        );
        assert!(env.is_empty());
    }

    #[test]
    fn kimi_launch_supports_explicit_model() {
        let spec = build_launch_spec(
            &detected(AgentKind::Kimi),
            LaunchOptions {
                model: "kimi-code/k3".to_owned(),
                effort: None,
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, args, _) = spec.into_parts();
        assert_eq!(args, ["--model", "kimi-code/k3"]);
    }

    #[test]
    fn kimi_launch_passes_thinking_effort_through_env() {
        let spec = build_launch_spec(
            &detected(AgentKind::Kimi),
            LaunchOptions {
                model: "kimi-code/k3-256k".to_owned(),
                effort: Some(ReasoningEffort::Max),
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, args, env) = spec.into_parts();
        assert_eq!(args, ["--model", "kimi-code/k3-256k"]);
        assert_eq!(
            env,
            [("KIMI_MODEL_THINKING_EFFORT".to_owned(), "max".to_owned())]
        );
    }

    #[test]
    fn kimi_boolean_thinking_models_offer_on_off_instead_of_levels() {
        for model in [
            "kimi-code/kimi-for-coding",
            "kimi-code/kimi-for-coding-highspeed",
        ] {
            let levels = detected(AgentKind::Kimi).supported_efforts(model).to_vec();
            assert_eq!(
                levels,
                [ReasoningEffort::On, ReasoningEffort::Off],
                "{model}"
            );
            assert!(ReasoningEffort::is_thinking_toggle(&levels), "{model}");
        }
        assert!(!ReasoningEffort::is_thinking_toggle(
            detected(AgentKind::Kimi).supported_efforts("kimi-code/k3")
        ));
        assert!(!ReasoningEffort::is_thinking_toggle(
            detected(AgentKind::Claude).supported_efforts("opus")
        ));

        let spec = build_launch_spec(
            &detected(AgentKind::Kimi),
            LaunchOptions {
                model: "kimi-code/kimi-for-coding".to_owned(),
                effort: Some(ReasoningEffort::Off),
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, _, env) = spec.into_parts();
        assert_eq!(
            env,
            [("KIMI_MODEL_THINKING_EFFORT".to_owned(), "off".to_owned())]
        );
    }

    #[test]
    fn a_configured_model_missing_from_the_catalog_is_still_offered() {
        // Claude 설정의 `opus[1m]`처럼 내장 목록에 없는 값이 실제로 존재한다. 그대로
        // 두면 런처가 사용자가 설정한 모델 대신 목록 첫 항목을 조용히 띄운다.
        let mut models = AgentKind::Claude.builtin_model_choices();
        let known = models.len();
        assert!(find_model(&models, "opus[1m]").is_none());

        adopt_model(AgentKind::Claude, "opus[1m]", &mut models);

        assert_eq!(models.len(), known + 1);
        assert_eq!(models[0].value(), "opus[1m]");
        assert_eq!(models[0].efforts(), CLAUDE_EFFORTS);
        // 기본 강도가 없으면 UI가 첫 단계(Low)를 골라 CLI 단독 실행보다 낮아진다.
        assert_eq!(models[0].default_effort(), Some(ReasoningEffort::High));
        // 실행 계약도 이 값을 받아들여야 한다.
        let agent = DetectedAgent {
            kind: AgentKind::Claude,
            executable: PathBuf::from("/opt/deppy/claude"),
            launch_path: None,
            models,
            default_model: Some("opus[1m]".to_owned()),
        };
        assert_eq!(agent.initial_model(), "opus[1m]");
        let spec = build_launch_spec(
            &agent,
            LaunchOptions {
                model: "opus[1m]".to_owned(),
                effort: Some(ReasoningEffort::High),
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, args, _) = spec.into_parts();
        assert_eq!(args, ["--model", "opus[1m]", "--effort", "high"]);
    }

    #[test]
    fn adopting_a_model_does_not_displace_a_catalog_entry_past_the_cap() {
        // 카탈로그가 상한을 채운 상태에서 설정 모델을 넣을 때, 넣고 나서 자르면 목록 끝의
        // 진짜 항목이 밀려난다. 그게 사용자가 고른 모델이면 다음 새로고침에 말없이 바뀐다.
        let mut models: Vec<ModelChoice> = (0..MODELS_PER_AGENT_MAX)
            .filter_map(|index| {
                ModelChoice::new(&format!("m{index}"), "", CLAUDE_EFFORTS.to_vec(), None)
            })
            .collect();
        assert_eq!(models.len(), MODELS_PER_AGENT_MAX);
        let last_before = models[MODELS_PER_AGENT_MAX - 1].value().to_owned();

        adopt_model(AgentKind::Claude, "opus[1m]", &mut models);

        assert_eq!(models.len(), MODELS_PER_AGENT_MAX);
        assert_eq!(models[0].value(), "opus[1m]");
        assert!(
            find_model(&models, &last_before).is_none(),
            "상한이 있으니 하나는 빠지지만, 빠지는 자리는 결정적이어야 한다"
        );
        // 이미 목록에 있으면 아무것도 바뀌지 않는다.
        let before = models.clone();
        adopt_model(AgentKind::Claude, "opus[1m]", &mut models);
        assert_eq!(models, before);
    }

    #[test]
    fn grok_launch_uses_its_own_effort_and_approval_flags() {
        let spec = build_launch_spec(
            &detected(AgentKind::Grok),
            LaunchOptions {
                model: "grok-4.5".to_owned(),
                effort: Some(ReasoningEffort::High),
                yolo: true,
            },
            None,
        )
        .unwrap();
        let (_, _, args, env) = spec.into_parts();
        assert_eq!(
            args,
            [
                "--always-approve",
                "--model",
                "grok-4.5",
                "--reasoning-effort",
                "high",
            ]
        );
        assert!(env.is_empty());

        // grok-4.5는 xhigh를 광고하지 않으므로 실행 계약이 막는다.
        assert!(matches!(
            build_launch_spec(
                &detected(AgentKind::Grok),
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

    #[test]
    fn qwen_offers_models_but_no_effort_because_its_cli_has_no_flag() {
        let qwen = detected(AgentKind::QwenCode);
        assert!(!qwen.models().is_empty());
        assert!(qwen.supported_efforts("coder-model").is_empty());
        assert!(matches!(
            build_launch_spec(
                &qwen,
                LaunchOptions {
                    model: "coder-model".to_owned(),
                    effort: Some(ReasoningEffort::High),
                    yolo: false,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedEffort)
        ));
    }

    #[test]
    fn claude_models_report_their_catalog_default_effort() {
        for model in ["sonnet", "opus", "fable"] {
            assert_eq!(
                find_model(&AgentKind::Claude.builtin_model_choices(), model)
                    .and_then(ModelChoice::default_effort),
                Some(ReasoningEffort::High),
                "{model}"
            );
        }
    }

    #[test]
    fn kimi_rejects_an_effort_the_model_does_not_declare() {
        assert!(matches!(
            build_launch_spec(
                &detected(AgentKind::Kimi),
                LaunchOptions {
                    model: "kimi-code/k3".to_owned(),
                    effort: Some(ReasoningEffort::Medium),
                    yolo: false,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedEffort)
        ));
    }

    #[test]
    fn model_capabilities_expose_bounded_unique_selectable_values() {
        for kind in AgentKind::ALL {
            let choices = kind.builtin_model_choices();
            assert_eq!(kind.supports_model(), !choices.is_empty(), "{}", kind.id());
            assert!(choices.len() <= 16, "{}", kind.id());
            let mut values = HashSet::new();
            for choice in &choices {
                assert!(!choice.value().is_empty(), "{}", kind.id());
                assert!(choice.value().len() <= MODEL_MAX_BYTES, "{}", kind.id());
                assert!(
                    !choice.value().bytes().any(|byte| byte.is_ascii_control()),
                    "{}",
                    kind.id()
                );
                assert!(!choice.label().is_empty(), "{}", kind.id());
                assert!(
                    choice
                        .default_effort()
                        .is_none_or(|effort| choice.efforts().contains(&effort)),
                    "{}:{}",
                    kind.id(),
                    choice.value()
                );
                for (index, effort) in choice.efforts().iter().enumerate() {
                    assert!(
                        !choice.efforts()[..index].contains(effort),
                        "{}:{}",
                        kind.id(),
                        choice.value()
                    );
                }
                assert!(values.insert(choice.value().to_owned()), "{}", kind.id());
            }
        }
    }

    #[test]
    fn codex_reasoning_capabilities_follow_the_selected_model() {
        assert_eq!(
            detected(AgentKind::Codex).supported_efforts("gpt-5.6-sol"),
            CODEX_EFFORTS_ULTRA
        );
        assert_eq!(
            find_model(&AgentKind::Codex.builtin_model_choices(), "gpt-5.6-sol")
                .and_then(ModelChoice::default_effort),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            detected(AgentKind::Codex).supported_efforts("gpt-5.6-luna"),
            CODEX_EFFORTS_MAX
        );
        assert_eq!(
            detected(AgentKind::Codex).supported_efforts("gpt-5.4"),
            CODEX_EFFORTS_XHIGH
        );
        assert_eq!(
            find_model(
                &AgentKind::Codex.builtin_model_choices(),
                "gpt-5.3-codex-spark"
            )
            .and_then(ModelChoice::default_effort),
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn codex_launch_rejects_an_effort_the_selected_model_does_not_support() {
        assert!(matches!(
            build_launch_spec(
                &detected(AgentKind::Codex),
                LaunchOptions {
                    model: "gpt-5.6-luna".to_owned(),
                    effort: Some(ReasoningEffort::Ultra),
                    yolo: false,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedEffort)
        ));
        let spec = build_launch_spec(
            &detected(AgentKind::Codex),
            LaunchOptions {
                model: "gpt-5.6-sol".to_owned(),
                effort: Some(ReasoningEffort::Ultra),
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, args, _) = spec.into_parts();
        assert_eq!(
            args,
            [
                "--model",
                "gpt-5.6-sol",
                "--config",
                "model_reasoning_effort=\"ultra\""
            ]
        );
    }

    #[test]
    fn detected_launch_path_is_forwarded_to_the_agent() {
        let agent = DetectedAgent {
            kind: AgentKind::Kimi,
            executable: PathBuf::from("/home/test/.kimi-code/bin/kimi"),
            launch_path: Some(Arc::from(
                "/home/test/.kimi-code/bin:/home/test/.nvm/versions/node/v24/bin:/usr/bin",
            )),
            models: AgentKind::Kimi.builtin_model_choices(),
            default_model: None,
        };
        let spec = build_launch_spec(
            &agent,
            LaunchOptions {
                model: String::new(),
                effort: None,
                yolo: false,
            },
            None,
        )
        .unwrap();
        let (_, _, _, env) = spec.into_parts();
        assert_eq!(env[0].0, "PATH");
        assert!(env[0].1.contains(".nvm/versions/node/v24/bin"));
    }

    #[test]
    fn unsupported_options_fail_closed() {
        let opencode = detected(AgentKind::OpenCode);
        assert!(matches!(
            build_launch_spec(
                &opencode,
                LaunchOptions {
                    model: String::new(),
                    effort: None,
                    yolo: true,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedYolo)
        ));
        assert!(matches!(
            build_launch_spec(
                &opencode,
                LaunchOptions {
                    model: "model".to_owned(),
                    effort: None,
                    yolo: false,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedModel)
        ));
        assert!(matches!(
            build_launch_spec(
                &detected(AgentKind::Codex),
                LaunchOptions {
                    model: "unknown-codex-model".to_owned(),
                    effort: None,
                    yolo: false,
                },
                None,
            ),
            Err(LaunchSpecErrorCode::UnsupportedModel)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn agent_exit_returns_to_an_interactive_shell_without_requoting_arguments() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let (command, args) = wrap_agent_then_shell(
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), "printf 'agent-done\\n'".to_owned()],
        );
        assert_eq!(command, "/bin/sh");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], AGENT_THEN_SHELL_SCRIPT);
        assert_eq!(args[2], "deppy-agent-session");
        assert_eq!(args[3], "/bin/sh");
        assert_eq!(args[4..], ["-c", "printf 'agent-done\\n'"]);

        let mut child = Command::new(command)
            .args(args)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"printf 'shell-ready\\n'\nexit\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("agent-done\n"), "{stdout:?}");
        assert!(stdout.contains("shell-ready\n"), "{stdout:?}");
    }

    #[cfg(unix)]
    #[test]
    fn executable_detection_requires_an_executable_regular_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "deppy-agent-detect-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("codex");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_executable("codex", std::slice::from_ref(&root)).is_none());
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_executable("codex", std::slice::from_ref(&root)),
            Some(binary)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_config_id에서_agent_kind를_복원한다() {
        for kind in AgentKind::ALL {
            assert_eq!(
                AgentKind::from_stable_config_id(kind.stable_config_id()),
                Some(kind)
            );
        }
        assert_eq!(AgentKind::from_stable_config_id("custom-agent"), None);
    }

    #[test]
    fn version_manager_paths_are_bounded_and_discoverable() {
        let home = std::env::temp_dir().join(format!(
            "deppy-agent-manager-paths-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let nvm = home.join(".nvm/versions/node/v24.0.0/bin");
        std::fs::create_dir_all(&nvm).unwrap();
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        push_versioned_detection_paths(
            &mut paths,
            &mut seen,
            &home.join(".nvm/versions/node"),
            Path::new("bin"),
        );
        assert_eq!(paths, [nvm]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn deppy_shim_directory_is_excluded_from_detection() {
        let home = std::env::temp_dir().join(format!(
            "deppy-agent-shim-exclusion-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let real = home.join(".local/bin");
        let shim = home.join(".deppy-sijo/shims");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&shim).unwrap();
        let mut paths = vec![shim.clone(), real.clone()];
        exclude_detection_directory(&mut paths, &shim);
        assert_eq!(paths, [real]);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// 다른 도구(cmux 등)가 세션마다 PATH 맨 앞에 까는 hook shim은 OS 임시 디렉터리
    /// 아래에 산다 — 실제 설치된 CLI가 거기 있을 리 없다. 이걸 "진짜 codex"로 오인해
    /// `DEPPY_AGENT_EXECUTABLE`로 넘기면, 우리 shim이 그 위에 우리 hook 인자를 또 얹고
    /// 그 shim이 다시 자기 hook을 넣어 `--dangerously-bypass-hook-trust`가 두 번
    /// 전달된다(실측 재현: cmux의 codex wrapper).
    #[test]
    fn 임시_디렉터리_아래_path_항목은_감지에서_제외한다() {
        let temp_shim =
            std::env::temp_dir().join("cmux-cli-shims/00000000-0000-0000-0000-000000000000");
        let real = PathBuf::from("/opt/homebrew/bin");
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        push_path_env_entry(&mut paths, &mut seen, temp_shim);
        push_path_env_entry(&mut paths, &mut seen, real.clone());
        assert_eq!(
            paths,
            [real],
            "임시 디렉터리 아래 PATH 항목(다른 도구의 세션별 hook shim)은 실제 설치 위치가 아니므로 걸러야 한다"
        );
    }
}
