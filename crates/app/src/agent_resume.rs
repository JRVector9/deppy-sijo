//! 세션 「이어가기」에 필요한 CLI 인자 판정 — 순수 함수, I/O 없음.
//!
//! 앱을 껐다 켜면 에이전트 pane은 열람 전용으로 복원된다(프로세스는 안 살림). 사용자가
//! 「다시 실행」을 누르면 DB `sessions` 행의 `args_json`(저장된 실행 인자) 뒤에 이 모듈이
//! 돌려주는 인자를 덧붙여 재실행한다 — agent_shim.rs의 shim이 그 뒤를 `"$@"`로 이어받는다.
//!
//! CLI마다 이어가기 플래그의 문법·위치 제약이 달라 `--help` 실측 없이 추측으로 넣으면
//! 시작 자체가 거부될 수 있다(오늘 겪은 codex `--dangerously-bypass-hook-trust` 중복 사건
//! 참고 — 추측 인자 하나가 에이전트를 통째로 못 띄우게 만들었다). 그래서 표에 없는
//! agent_id, 그리고 실측하지 못한 CLI는 무조건 빈 벡터를 돌려준다 — 이어가기 실패보다
//! 새 대화로 시작하는 쪽이 낫다.

const NATIVE_SESSION_ID_BYTES_MAX: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeMode {
    Exact,
    RecentInCwd,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchivedResumePresentation {
    Checking,
    Exact,
    RecentInCwd,
    Unsupported,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResumePlan {
    pub(crate) mode: ResumeMode,
    pub(crate) extra_args: Vec<String>,
}

impl ResumePlan {
    pub(crate) fn into_extra_args(self) -> Vec<String> {
        self.extra_args
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeProvider {
    Claude,
    Codex,
    Kimi,
    Grok,
    QwenCode,
}

impl ResumeProvider {
    fn from_agent_id(agent_id: &str) -> Option<Self> {
        match agent_id {
            "deppy-builtin-claude" => Some(Self::Claude),
            "deppy-builtin-codex" => Some(Self::Codex),
            "deppy-builtin-kimi" => Some(Self::Kimi),
            "deppy-builtin-grok" => Some(Self::Grok),
            "deppy-builtin-qwen-code" => Some(Self::QwenCode),
            _ => None,
        }
    }

    fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "kimi" => Some(Self::Kimi),
            "grok" => Some(Self::Grok),
            "qwen-code" | "qwen" => Some(Self::QwenCode),
            _ => None,
        }
    }

    fn recent_args(self) -> Vec<String> {
        match self {
            // Grok 1.0.13 `--help` 실측: `-c, --continue`는 현재 cwd의 최근 세션.
            Self::Claude | Self::Kimi | Self::Grok => vec!["-c".to_owned()],
            Self::Codex => vec!["resume".to_owned(), "--last".to_owned()],
            Self::QwenCode => vec!["--continue".to_owned()],
        }
    }

    fn exact_args(self, session_id: &str) -> Vec<String> {
        match self {
            // Grok 1.0.13 `--help` 실측: `--resume <SESSION_ID_OR_TITLE>`에서 UUID는
            // 항상 세션 ID로 해석한다.
            Self::Claude | Self::Grok | Self::QwenCode => {
                vec!["--resume".to_owned(), session_id.to_owned()]
            }
            Self::Codex => vec!["resume".to_owned(), session_id.to_owned()],
            Self::Kimi => vec!["--session".to_owned(), session_id.to_owned()],
        }
    }
}

fn native_session_id_is_valid(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= NATIVE_SESSION_ID_BYTES_MAX
        && !session_id.bytes().any(|byte| byte.is_ascii_control())
}

/// 저장된 agent 설정과 선택적인 CLI-native binding으로 정확/최근 재개 계획을 만든다.
/// built-in agent의 provider와 binding kind가 다르면 오래되거나 잘못된 binding으로 보고
/// 무시한다. custom agent는 검증된 binding kind가 유일한 provider 근거다.
pub(crate) fn resume_plan(agent_id: &str, binding: Option<(&str, &str)>) -> ResumePlan {
    let persisted_provider = ResumeProvider::from_agent_id(agent_id);
    let binding = binding.and_then(|(kind, session_id)| {
        let provider = ResumeProvider::from_kind(kind)?;
        native_session_id_is_valid(session_id).then_some((provider, session_id))
    });
    let provider = persisted_provider.or_else(|| binding.map(|(provider, _)| provider));
    let Some(provider) = provider else {
        return ResumePlan {
            mode: ResumeMode::Unsupported,
            extra_args: Vec::new(),
        };
    };
    if let Some((binding_provider, session_id)) = binding
        && binding_provider == provider
    {
        return ResumePlan {
            mode: ResumeMode::Exact,
            extra_args: provider.exact_args(session_id),
        };
    }
    ResumePlan {
        mode: ResumeMode::RecentInCwd,
        extra_args: provider.recent_args(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_resume은_검증된_provider와_native_session_id를_쓴다() {
        let cases = [
            (
                "deppy-builtin-claude",
                "claude",
                vec!["--resume", "claude-session"],
            ),
            (
                "deppy-builtin-codex",
                "codex",
                vec!["resume", "codex-session"],
            ),
            (
                "deppy-builtin-kimi",
                "kimi",
                vec!["--session", "kimi-session"],
            ),
            (
                "deppy-builtin-grok",
                "grok",
                vec!["--resume", "grok-session"],
            ),
            (
                "deppy-builtin-qwen-code",
                "qwen-code",
                vec!["--resume", "qwen-session"],
            ),
        ];

        for (agent_id, kind, expected) in cases {
            let plan = resume_plan(agent_id, Some((kind, expected.last().unwrap())));
            assert_eq!(plan.mode, ResumeMode::Exact, "{agent_id}");
            assert_eq!(
                plan.extra_args,
                expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "{agent_id}"
            );
        }
    }

    #[test]
    fn exact_token이_없으면_provider의_최근_폴더_재개로_강등한다() {
        let cases = [
            ("deppy-builtin-claude", vec!["-c"]),
            ("deppy-builtin-codex", vec!["resume", "--last"]),
            ("deppy-builtin-kimi", vec!["-c"]),
            ("deppy-builtin-grok", vec!["-c"]),
            ("deppy-builtin-qwen-code", vec!["--continue"]),
        ];

        for (agent_id, expected) in cases {
            let plan = resume_plan(agent_id, None);
            assert_eq!(plan.mode, ResumeMode::RecentInCwd, "{agent_id}");
            assert_eq!(
                plan.extra_args,
                expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "{agent_id}"
            );
        }
    }

    #[test]
    fn 잘못된_exact_binding은_내장_provider의_최근_재개를_바꾸지_않는다() {
        let mismatched = resume_plan(
            "deppy-builtin-kimi",
            Some(("claude", "wrong-provider-token")),
        );
        assert_eq!(mismatched.mode, ResumeMode::RecentInCwd);
        assert_eq!(mismatched.extra_args, ["-c"]);

        for invalid in ["", "bad\nsession", &"x".repeat(1025)] {
            let plan = resume_plan("deppy-builtin-codex", Some(("codex", invalid)));
            assert_eq!(plan.mode, ResumeMode::RecentInCwd);
            assert_eq!(plan.extra_args, ["resume", "--last"]);
        }
    }

    #[test]
    fn custom_agent는_검증된_binding이_있을_때만_exact_resume한다() {
        let exact = resume_plan("custom-agent", Some(("claude", "custom-session")));
        assert_eq!(exact.mode, ResumeMode::Exact);
        assert_eq!(exact.extra_args, ["--resume", "custom-session"]);

        let unsupported = resume_plan("custom-agent", None);
        assert_eq!(unsupported.mode, ResumeMode::Unsupported);
        assert!(unsupported.extra_args.is_empty());
    }
}
