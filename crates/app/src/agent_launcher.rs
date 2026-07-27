use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const EXECUTABLE_PATH_MAX_BYTES: usize = 4 * 1024;
const MODEL_MAX_BYTES: usize = 256;
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
    Cursor,
    Copilot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    value: &'static str,
    label: &'static str,
    efforts: &'static [ReasoningEffort],
    default_effort: Option<ReasoningEffort>,
}

impl ModelChoice {
    pub(crate) const fn value(self) -> &'static str {
        self.value
    }

    pub(crate) const fn label(self) -> &'static str {
        self.label
    }

    pub(crate) const fn efforts(self) -> &'static [ReasoningEffort] {
        self.efforts
    }

    pub(crate) const fn default_effort(self) -> Option<ReasoningEffort> {
        self.default_effort
    }
}

impl AgentKind {
    pub(crate) const ALL: [Self; 11] = [
        Self::Claude,
        Self::Codex,
        Self::OpenCode,
        Self::Gemini,
        Self::Aider,
        Self::Goose,
        Self::Amp,
        Self::Kimi,
        Self::QwenCode,
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
            Self::Cursor => "deppy-builtin-cursor",
            Self::Copilot => "deppy-builtin-copilot",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini CLI",
            Self::Aider => "Aider",
            Self::Goose => "Goose",
            Self::Amp => "Amp",
            Self::Kimi => "Kimi CLI",
            Self::QwenCode => "Qwen Code",
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
            Self::Cursor => "cursor-agent",
            Self::Copilot => "copilot",
        }
    }

    pub(crate) const fn supports_model(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Kimi)
    }

    pub(crate) const fn supported_models(self) -> &'static [ModelChoice] {
        match self {
            Self::Claude => &[
                ModelChoice {
                    value: "sonnet",
                    label: "Sonnet",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: None,
                },
                ModelChoice {
                    value: "opus",
                    label: "Opus",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: None,
                },
                ModelChoice {
                    value: "fable",
                    label: "Fable",
                    efforts: CLAUDE_EFFORTS,
                    default_effort: None,
                },
            ],
            Self::Codex => &[
                ModelChoice {
                    value: "gpt-5.6-sol",
                    label: "GPT-5.6-Sol",
                    efforts: CODEX_EFFORTS_ULTRA,
                    default_effort: Some(ReasoningEffort::Low),
                },
                ModelChoice {
                    value: "gpt-5.6-terra",
                    label: "GPT-5.6-Terra",
                    efforts: CODEX_EFFORTS_ULTRA,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                ModelChoice {
                    value: "gpt-5.6-luna",
                    label: "GPT-5.6-Luna",
                    efforts: CODEX_EFFORTS_MAX,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                ModelChoice {
                    value: "gpt-5.5",
                    label: "GPT-5.5",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                ModelChoice {
                    value: "gpt-5.4",
                    label: "GPT-5.4",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                ModelChoice {
                    value: "gpt-5.4-mini",
                    label: "GPT-5.4-Mini",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::Medium),
                },
                ModelChoice {
                    value: "gpt-5.3-codex-spark",
                    label: "GPT-5.3-Codex-Spark",
                    efforts: CODEX_EFFORTS_XHIGH,
                    default_effort: Some(ReasoningEffort::High),
                },
            ],
            Self::Kimi => &[
                ModelChoice {
                    value: "kimi-code/kimi-for-coding",
                    label: "K2.7 Coding",
                    efforts: &[],
                    default_effort: None,
                },
                ModelChoice {
                    value: "kimi-code/kimi-for-coding-highspeed",
                    label: "K2.7 Coding Highspeed",
                    efforts: &[],
                    default_effort: None,
                },
                ModelChoice {
                    value: "kimi-code/k3",
                    label: "K3",
                    efforts: &[],
                    default_effort: None,
                },
                ModelChoice {
                    value: "kimi-code/k3-256k",
                    label: "K3-256k",
                    efforts: &[],
                    default_effort: None,
                },
            ],
            _ => &[],
        }
    }

    pub(crate) const fn supports_yolo(self) -> bool {
        !matches!(self, Self::OpenCode)
    }

    pub(crate) const fn supports_deppy_shim(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    pub(crate) fn supported_efforts(self, model: &str) -> &'static [ReasoningEffort] {
        self.supported_models()
            .iter()
            .find(|choice| choice.value() == model)
            .map_or_else(
                || match self {
                    Self::Codex => CODEX_EFFORTS_XHIGH,
                    Self::Claude => CLAUDE_EFFORTS,
                    _ => &[],
                },
                |choice| choice.efforts(),
            )
    }

    pub(crate) fn default_effort(self, model: &str) -> Option<ReasoningEffort> {
        self.supported_models()
            .iter()
            .find(|choice| choice.value() == model)
            .and_then(|choice| choice.default_effort())
    }
}

pub(crate) fn is_builtin_config_id(id: &str) -> bool {
    AgentKind::ALL
        .into_iter()
        .any(|kind| kind.stable_config_id() == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
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
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DetectedAgent {
    kind: AgentKind,
    executable: PathBuf,
    launch_path: Option<Arc<str>>,
}

impl DetectedAgent {
    pub(crate) fn kind(&self) -> AgentKind {
        self.kind
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }
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
}

impl DetectionSnapshot {
    pub(crate) fn agents(&self) -> &[DetectedAgent] {
        &self.agents
    }

    pub(crate) fn find(&self, kind: AgentKind) -> Option<&DetectedAgent> {
        self.agents.iter().find(|agent| agent.kind == kind)
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
                })
                .collect(),
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

pub(crate) fn detect_installed_agents(excluded_directory: Option<&Path>) -> DetectionSnapshot {
    let paths = detection_paths(excluded_directory);
    let launch_path = launch_search_path(&paths);
    let agents = AgentKind::ALL
        .into_iter()
        .filter_map(|kind| {
            resolve_executable(kind.detect_command(), &paths).map(|executable| DetectedAgent {
                kind,
                executable,
                launch_path: launch_path.clone(),
            })
        })
        .collect();
    DetectionSnapshot { agents }
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
            || !agent
                .kind
                .supported_models()
                .iter()
                .any(|choice| choice.value() == model))
    {
        return Err(LaunchSpecErrorCode::UnsupportedModel);
    }
    if options
        .effort
        .is_some_and(|effort| !agent.kind.supported_efforts(model).contains(&effort))
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
            push_detection_path(&mut paths, &mut seen, path);
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
    fn model_capabilities_expose_bounded_unique_selectable_values() {
        for kind in AgentKind::ALL {
            let choices = kind.supported_models();
            assert_eq!(kind.supports_model(), !choices.is_empty(), "{}", kind.id());
            assert!(choices.len() <= 16, "{}", kind.id());
            let mut values = HashSet::new();
            for choice in choices {
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
                assert!(values.insert(choice.value()), "{}", kind.id());
            }
        }
    }

    #[test]
    fn codex_reasoning_capabilities_follow_the_selected_model() {
        assert_eq!(
            AgentKind::Codex.supported_efforts("gpt-5.6-sol"),
            CODEX_EFFORTS_ULTRA
        );
        assert_eq!(
            AgentKind::Codex.default_effort("gpt-5.6-sol"),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            AgentKind::Codex.supported_efforts("gpt-5.6-luna"),
            CODEX_EFFORTS_MAX
        );
        assert_eq!(
            AgentKind::Codex.supported_efforts("gpt-5.4"),
            CODEX_EFFORTS_XHIGH
        );
        assert_eq!(
            AgentKind::Codex.default_effort("gpt-5.3-codex-spark"),
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
}
