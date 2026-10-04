//! Bounded, provider-specific settings for the original reserved terminal task.
use crate::agent_launcher::{DetectionSnapshot, ReasoningEffort};
use crate::agent_surface::AgentProvider;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffortRequest {
    pub provider: AgentProvider,
    pub model: String,
    pub level: ReasoningEffort,
}

#[derive(Clone, Default)]
pub(crate) struct EffortContext {
    pub provider: Option<AgentProvider>,
    pub model: String,
    pub current: Option<String>,
    pub levels: Vec<ReasoningEffort>,
}

impl EffortContext {
    pub fn from_display(
        display: &crate::agent_detect::AgentDisplay,
        catalog: Option<&DetectionSnapshot>,
    ) -> Self {
        let provider = display.kind.into();
        let model = display.model.as_deref().unwrap_or_default();
        let levels = catalog
            .and_then(|catalog| {
                catalog.agents().iter().find(|agent| {
                    matches!(
                        (agent.kind(), provider),
                        (
                            crate::agent_launcher::AgentKind::Codex,
                            AgentProvider::Codex
                        ) | (
                            crate::agent_launcher::AgentKind::Claude,
                            AgentProvider::Claude
                        ) | (crate::agent_launcher::AgentKind::Kimi, AgentProvider::Kimi)
                            | (crate::agent_launcher::AgentKind::Grok, AgentProvider::Grok)
                    )
                })
            })
            .and_then(|agent| {
                agent.models().iter().find(|choice| {
                    choice.value() == model
                        || (provider == AgentProvider::Claude
                            && model.to_ascii_lowercase().contains(choice.value()))
                })
            })
            .map(|choice| choice.efforts().to_vec())
            .unwrap_or_default();
        Self {
            provider: Some(provider),
            model: model.chars().take(256).collect(),
            current: display
                .effort
                .as_ref()
                .map(|value| value.chars().take(32).collect()),
            levels: levels
                .into_iter()
                .filter(|level| match provider {
                    AgentProvider::Codex => {
                        !matches!(level, ReasoningEffort::On | ReasoningEffort::Off)
                    }
                    AgentProvider::Claude => !matches!(
                        level,
                        ReasoningEffort::Ultra | ReasoningEffort::On | ReasoningEffort::Off
                    ),
                    AgentProvider::Kimi => false,
                    AgentProvider::Grok => false,
                })
                .take(8)
                .collect(),
        }
    }

    pub fn request(&self, level: ReasoningEffort) -> Option<EffortRequest> {
        (self.levels.contains(&level)
            && !(self.provider == Some(AgentProvider::Codex) && level == ReasoningEffort::Ultra))
            .then(|| {
                Some(EffortRequest {
                    provider: self.provider?,
                    model: self.model.clone(),
                    level,
                })
            })
            .flatten()
    }

    pub fn plan(&self, request: &EffortRequest) -> Option<crate::pty_effort::EffortPlan> {
        use crate::pty_effort::{EffortPlan, EffortStep};
        if self.provider != Some(request.provider)
            || self.model != request.model
            || !self.levels.contains(&request.level)
            || self.model.is_empty()
        {
            return None;
        }
        let level = request.level.value();
        match request.provider {
            AgentProvider::Claude => Some(EffortPlan::Slash {
                line: format!("/effort {level}"),
                level,
            }),
            AgentProvider::Kimi => Some(EffortPlan::Slash {
                line: format!("/thinking {level}"),
                level,
            }),
            AgentProvider::Codex => {
                let current = self.current.as_deref()?;
                let from = self
                    .levels
                    .iter()
                    .position(|effort| effort.value().eq_ignore_ascii_case(current))?;
                let to = self
                    .levels
                    .iter()
                    .position(|effort| *effort == request.level)?;
                let step = if to > from {
                    EffortStep::Up
                } else {
                    EffortStep::Down
                };
                let EffortPlan::Keys(key) =
                    crate::pty_effort::plan(request.provider, step, Some(current)).ok()?
                else {
                    return None;
                };
                Some(EffortPlan::Keys(if from == to { Vec::new() } else { key }))
            }
            AgentProvider::Grok => None,
        }
    }
}

/// Fresh local CLI state is separate from historical transcript effort.
pub(crate) fn current_codex_effort(text: &str, context: &EffortContext) -> Option<ReasoningEffort> {
    text.lines().rev().take(5).find_map(|line| {
        if context.model.is_empty() || !line.contains(&context.model) {
            return None;
        }
        context.levels.iter().copied().find(|effort| {
            line.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|token| token.eq_ignore_ascii_case(effort.value()))
        })
    })
}

/// Only call with the operation-owned fresh output, never a screen/history snapshot.
pub(crate) fn acknowledged_in_reply(text: &str, request: &EffortRequest) -> bool {
    let text = crate::agent_model_probe::strip_ansi(text);
    let text = text
        .rsplit_once(&format!("/effort {}", request.level.value()))
        .map_or(text.as_str(), |(_, after)| after);
    text.lines().any(|line| {
        let line = line.trim().trim_start_matches(['•', '✓', '⎿']).trim();
        match request.provider {
            AgentProvider::Claude => {
                let Some(after) = line.strip_prefix("Set effort level to ") else {
                    return false;
                };
                after == request.level.value()
                    || after
                        .strip_prefix(request.level.value())
                        .is_some_and(|tail| tail.starts_with(" (saved as your default"))
            }
            AgentProvider::Kimi => false,
            AgentProvider::Codex | AgentProvider::Grok => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(provider: AgentProvider) -> EffortContext {
        EffortContext {
            provider: Some(provider),
            model: "actual-model".into(),
            current: Some("high".into()),
            levels: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ],
        }
    }
    #[test]
    fn terminal_acknowledgement_requires_cli_message_not_bare_word() {
        let request = context(AgentProvider::Claude)
            .request(ReasoningEffort::Low)
            .unwrap();
        assert!(!acknowledged_in_reply(
            "The agent says low effort is sufficient.",
            &request
        ));
        assert!(acknowledged_in_reply(
            "Set effort level to low (saved as your default for new sessions)",
            &request
        ));
        assert!(!acknowledged_in_reply(
            "Set effort level to lowest",
            &request
        ));
        assert!(!acknowledged_in_reply("Set effort level to high", &request));
        assert!(acknowledged_in_reply(
            "/effort low\r\n\x1b[32mSet effort level to low\x1b[0m",
            &request
        ));
        assert!(!acknowledged_in_reply(
            "Set effort level to low\r\n/effort low\r\n",
            &request
        ));
    }

    #[test]
    fn codex_fresh_screen_overrides_historical_turn_effort() {
        let mut context = context(AgentProvider::Codex);
        let request = context.request(ReasoningEffort::High).unwrap();
        let actual =
            current_codex_effort("actual-model medium · 50% context left", &context).unwrap();
        context.current = Some(actual.value().into());
        assert_eq!(
            context.plan(&request),
            Some(crate::pty_effort::EffortPlan::Keys(b"\x1b[1;2A".to_vec()))
        );
    }

    #[test]
    fn codex_preserves_ultra_in_calculation_and_can_lower_it() {
        let mut context = context(AgentProvider::Codex);
        context.levels.push(ReasoningEffort::Ultra);
        context.current = Some("ultra".into());
        let request = context.request(ReasoningEffort::High).unwrap();
        assert_eq!(
            context.plan(&request),
            Some(crate::pty_effort::EffortPlan::Keys(b"\x1b[1;2B".to_vec()))
        );
        assert!(context.request(ReasoningEffort::Ultra).is_none());
    }

    #[test]
    fn absolute_codex_effort_uses_csi_native_steps() {
        let context = context(AgentProvider::Codex);
        let request = context.request(ReasoningEffort::Low).unwrap();
        assert_eq!(
            context.plan(&request),
            Some(crate::pty_effort::EffortPlan::Keys(b"\x1b[1;2B".to_vec()))
        );
    }
    #[test]
    fn absolute_claude_effort_uses_verified_slash_command() {
        let context = context(AgentProvider::Claude);
        assert_eq!(
            context.plan(&context.request(ReasoningEffort::XHigh).unwrap()),
            Some(crate::pty_effort::EffortPlan::Slash {
                line: "/effort xhigh".into(),
                level: "xhigh"
            })
        );
    }
    #[test]
    fn changed_model_or_unknown_current_never_sends_guessed_keys() {
        let mut context = context(AgentProvider::Codex);
        let mut request = context.request(ReasoningEffort::Low).unwrap();
        request.model = "other-model".into();
        assert_eq!(context.plan(&request), None);
        request.model = context.model.clone();
        context.current = None;
        assert_eq!(context.plan(&request), None);
    }
}
