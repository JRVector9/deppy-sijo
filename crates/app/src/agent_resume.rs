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
    QwenCode,
}

impl ResumeProvider {
    fn from_agent_id(agent_id: &str) -> Option<Self> {
        match agent_id {
            "deppy-builtin-claude" => Some(Self::Claude),
            "deppy-builtin-codex" => Some(Self::Codex),
            "deppy-builtin-kimi" => Some(Self::Kimi),
            "deppy-builtin-qwen-code" => Some(Self::QwenCode),
            _ => None,
        }
    }

    fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "kimi" => Some(Self::Kimi),
            "qwen-code" | "qwen" => Some(Self::QwenCode),
            _ => None,
        }
    }

    fn recent_args(self) -> Vec<String> {
        match self {
            Self::Claude | Self::Kimi => vec!["-c".to_owned()],
            Self::Codex => vec!["resume".to_owned(), "--last".to_owned()],
            Self::QwenCode => vec!["--continue".to_owned()],
        }
    }

    fn exact_args(self, session_id: &str) -> Vec<String> {
        match self {
            Self::Claude | Self::QwenCode => {
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

/// 레거시 호출부용 최근-directory resume 인자. 명시적 전략을 쓰는 archived-pane 경로가
/// 연결될 때까지 기존 동작을 유지한다.
pub fn resume_args(agent_id: &str) -> Vec<String> {
    match agent_id {
        // claude 2.1.227, `claude --help` 실측: `-c, --continue  Continue the most
        // recent conversation in the current directory`. 인자를 받지 않는 불리언
        // 플래그라 저장된 `--model`/`--effort` 뒤에 붙어도 파싱에 영향이 없다 — 실측:
        // `claude --model opus --effort high -c --help` → exit 0.
        "deppy-builtin-claude" | "deppy-builtin-codex" | "deppy-builtin-kimi" => {
            resume_plan(agent_id, None).into_extra_args()
        }

        // codex-cli 0.147.0. `codex --help`에는 `--effort`가 아예 없다 — deppy가
        // effort를 넘길 때도 `--config model_reasoning_effort="..."`를 쓴다
        // (agent_launcher.rs build_launch_spec). resume은 서브커맨드라
        // `codex [OPTIONS] <COMMAND>` 규칙상 전역 옵션(--model, --config) *뒤에*
        // 와야 하는데, 저장된 인자가 바로 그 전역 옵션들이다. 실측으로 위치 규칙을
        // 확인했다(실제 세션은 만들지 않고 --help로 파싱만 확인):
        // `codex --model gpt-5.1-codex-max --config 'model_reasoning_effort="high"'
        //  resume --last --help` → exit 0, `codex resume --help`와 동일한 출력.
        // `--last`는 세션 id 없이 "가장 최근 세션"을 고른다(`codex resume --help`).
        // Kimi CLI 0.34.0, `~/.kimi-code/bin/kimi --help` 실측: `-c, --continue
        // Continue the previous session for the working directory.` claude와 같은
        // 형태의 불리언 플래그. 실측: `kimi --model kimi-code/k3 -c --help` → exit 0.
        // grok: 이 작업 환경에는 실제 xAI grok CLI가 설치돼 있지 않아 `--help`를 실행할
        // 수 없었다(PATH의 `grok`은 다른 도구의 wrapper로, 실행하면 "grok not found in
        // PATH"를 반환한다). 실측하지 못한 채 넣지 않는다.
        _ => Vec::new(),
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

    #[test]
    fn 표에_있는_에이전트는_실측된_이어가기_인자를_돌려준다() {
        assert_eq!(resume_args("deppy-builtin-claude"), vec!["-c".to_owned()]);
        assert_eq!(
            resume_args("deppy-builtin-codex"),
            vec!["resume".to_owned(), "--last".to_owned()]
        );
        assert_eq!(resume_args("deppy-builtin-kimi"), vec!["-c".to_owned()]);
    }

    #[test]
    fn 실측하지_못한_grok과_미지원_agent_id는_빈_벡터다() {
        // grok: --help를 실행할 CLI 자체가 이 환경에 없어 실측하지 못했다.
        assert!(resume_args("deppy-builtin-grok").is_empty());
        // 표에 아예 없는 agent_id(다른 빌트인, 오타, 빈 문자열)도 전부 빈 벡터.
        assert!(resume_args("deppy-builtin-opencode").is_empty());
        assert!(resume_args("deppy-builtin-gemini").is_empty());
        assert!(resume_args("unknown-agent").is_empty());
        assert!(resume_args("").is_empty());
    }
}
