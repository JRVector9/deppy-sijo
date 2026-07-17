//! 사용자 지정 전역 단축키.
//!
//! config에는 egui 타입을 직접 직렬화하지 않고 portable 문자열만 저장한다. 기본값과
//! 다른 항목만 override로 남겨 새 동작을 추가해도 기존 사용자가 새 기본값을 자동으로
//! 받는다.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::ShortcutsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShortcutAction {
    ToggleSidebar,
    OpenEnvironment,
    OpenAgents,
    OpenActivity,
    OpenNotifications,
    NewShell,
    ClosePane,
    SplitVertical,
    SplitHorizontal,
    FocusNextPane,
    FocusPreviousPane,
    NextWorkspace,
    PreviousWorkspace,
    IncreaseTerminalFont,
    DecreaseTerminalFont,
    TerminalSearch,
    /// 스크롤백에서 맨 아래(라이브 화면)로 복귀 (⌘↓ 기본 — pane 메뉴와 동일 동작)
    ScrollToBottom,
    /// 이전 프롬프트 마크(OSC 133)로 점프 (⌘⇧↑ 기본 — 셸 통합 1단계)
    PromptJumpPrev,
    /// 다음 프롬프트 마크(OSC 133)로 점프 (⌘⇧↓ 기본)
    PromptJumpNext,
    /// 하단 도크 컴포저 포커스+펼침 (⌘J 기본). 이미 포커스면 컴포저 안에서 접는다
    /// (전역 단축키는 text edit 포커스 중 비활성 — 접기는 composer가 직접 처리).
    FocusComposer,
    ClearRenderCaches,
    PreviousAgent,
    NextAgent,
    FocusAgentInput,
    NewStructuredAgent,
    InterruptAgent,
    ApproveAgent,
    RejectAgent,
    IncreaseAgentEffort,
    DecreaseAgentEffort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutGroup {
    Navigation,
    Terminal,
    Workspace,
    Agent,
}

impl ShortcutGroup {
    pub const ALL: [Self; 4] = [
        Self::Navigation,
        Self::Terminal,
        Self::Workspace,
        Self::Agent,
    ];

    pub const fn title_key(self) -> &'static str {
        match self {
            Self::Navigation => "shortcuts.group.navigation",
            Self::Terminal => "shortcuts.group.terminal",
            Self::Workspace => "shortcuts.group.workspace",
            Self::Agent => "shortcuts.group.agent",
        }
    }
}

impl ShortcutAction {
    pub const ALL: [Self; 30] = [
        Self::ToggleSidebar,
        Self::OpenEnvironment,
        Self::OpenAgents,
        Self::OpenActivity,
        Self::OpenNotifications,
        Self::NewShell,
        Self::ClosePane,
        Self::SplitVertical,
        Self::SplitHorizontal,
        Self::FocusNextPane,
        Self::FocusPreviousPane,
        Self::NextWorkspace,
        Self::PreviousWorkspace,
        Self::IncreaseTerminalFont,
        Self::DecreaseTerminalFont,
        Self::TerminalSearch,
        Self::ScrollToBottom,
        Self::PromptJumpPrev,
        Self::PromptJumpNext,
        Self::FocusComposer,
        Self::ClearRenderCaches,
        Self::PreviousAgent,
        Self::NextAgent,
        Self::FocusAgentInput,
        Self::NewStructuredAgent,
        Self::InterruptAgent,
        Self::ApproveAgent,
        Self::RejectAgent,
        Self::IncreaseAgentEffort,
        Self::DecreaseAgentEffort,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::ToggleSidebar => "toggle_sidebar",
            Self::OpenEnvironment => "open_environment",
            Self::OpenAgents => "open_agents",
            Self::OpenActivity => "open_activity",
            Self::OpenNotifications => "open_notifications",
            Self::NewShell => "new_shell",
            Self::ClosePane => "close_pane",
            Self::SplitVertical => "split_vertical",
            Self::SplitHorizontal => "split_horizontal",
            Self::FocusNextPane => "focus_next_pane",
            Self::FocusPreviousPane => "focus_previous_pane",
            Self::NextWorkspace => "next_workspace",
            Self::PreviousWorkspace => "previous_workspace",
            Self::IncreaseTerminalFont => "increase_terminal_font",
            Self::DecreaseTerminalFont => "decrease_terminal_font",
            Self::TerminalSearch => "terminal_search",
            Self::ScrollToBottom => "scroll_to_bottom",
            Self::PromptJumpPrev => "prompt_jump_prev",
            Self::PromptJumpNext => "prompt_jump_next",
            Self::FocusComposer => "focus_composer",
            Self::ClearRenderCaches => "clear_render_caches",
            Self::PreviousAgent => "previous_agent",
            Self::NextAgent => "next_agent",
            Self::FocusAgentInput => "focus_agent_input",
            Self::NewStructuredAgent => "new_structured_agent",
            Self::InterruptAgent => "interrupt_agent",
            Self::ApproveAgent => "approve_agent",
            Self::RejectAgent => "reject_agent",
            Self::IncreaseAgentEffort => "increase_agent_effort",
            Self::DecreaseAgentEffort => "decrease_agent_effort",
        }
    }

    pub const fn title_key(self) -> &'static str {
        match self {
            Self::ToggleSidebar => "shortcuts.action.toggle_sidebar",
            Self::OpenEnvironment => "shortcuts.action.open_environment",
            Self::OpenAgents => "shortcuts.action.open_agents",
            Self::OpenActivity => "shortcuts.action.open_activity",
            Self::OpenNotifications => "shortcuts.action.open_notifications",
            Self::NewShell => "shortcuts.action.new_shell",
            Self::ClosePane => "shortcuts.action.close_pane",
            Self::SplitVertical => "shortcuts.action.split_vertical",
            Self::SplitHorizontal => "shortcuts.action.split_horizontal",
            Self::FocusNextPane => "shortcuts.action.focus_next_pane",
            Self::FocusPreviousPane => "shortcuts.action.focus_previous_pane",
            Self::NextWorkspace => "shortcuts.action.next_workspace",
            Self::PreviousWorkspace => "shortcuts.action.previous_workspace",
            Self::IncreaseTerminalFont => "shortcuts.action.increase_terminal_font",
            Self::DecreaseTerminalFont => "shortcuts.action.decrease_terminal_font",
            Self::TerminalSearch => "shortcuts.action.terminal_search",
            Self::ScrollToBottom => "shortcuts.action.scroll_to_bottom",
            Self::PromptJumpPrev => "shortcuts.action.prompt_jump_prev",
            Self::PromptJumpNext => "shortcuts.action.prompt_jump_next",
            Self::FocusComposer => "shortcuts.action.focus_composer",
            Self::ClearRenderCaches => "shortcuts.action.clear_render_caches",
            Self::PreviousAgent => "shortcuts.action.previous_agent",
            Self::NextAgent => "shortcuts.action.next_agent",
            Self::FocusAgentInput => "shortcuts.action.focus_agent_input",
            Self::NewStructuredAgent => "shortcuts.action.new_structured_agent",
            Self::InterruptAgent => "shortcuts.action.interrupt_agent",
            Self::ApproveAgent => "shortcuts.action.approve_agent",
            Self::RejectAgent => "shortcuts.action.reject_agent",
            Self::IncreaseAgentEffort => "shortcuts.action.increase_agent_effort",
            Self::DecreaseAgentEffort => "shortcuts.action.decrease_agent_effort",
        }
    }

    pub const fn description_key(self) -> &'static str {
        match self.group() {
            ShortcutGroup::Navigation => "shortcuts.desc.navigation",
            ShortcutGroup::Terminal => "shortcuts.desc.terminal",
            ShortcutGroup::Workspace => "shortcuts.desc.workspace",
            ShortcutGroup::Agent => "shortcuts.desc.agent",
        }
    }

    pub const fn group(self) -> ShortcutGroup {
        match self {
            Self::ToggleSidebar
            | Self::OpenEnvironment
            | Self::OpenActivity
            | Self::OpenNotifications => ShortcutGroup::Navigation,
            Self::NewShell
            | Self::ClosePane
            | Self::SplitVertical
            | Self::SplitHorizontal
            | Self::FocusNextPane
            | Self::FocusPreviousPane
            | Self::IncreaseTerminalFont
            | Self::DecreaseTerminalFont
            | Self::TerminalSearch
            | Self::ScrollToBottom
            | Self::PromptJumpPrev
            | Self::PromptJumpNext
            | Self::FocusComposer => ShortcutGroup::Terminal,
            Self::NextWorkspace | Self::PreviousWorkspace | Self::ClearRenderCaches => {
                ShortcutGroup::Workspace
            }
            Self::OpenAgents
            | Self::PreviousAgent
            | Self::NextAgent
            | Self::FocusAgentInput
            | Self::NewStructuredAgent
            | Self::InterruptAgent
            | Self::ApproveAgent
            | Self::RejectAgent
            | Self::IncreaseAgentEffort
            | Self::DecreaseAgentEffort => ShortcutGroup::Agent,
        }
    }

    fn default_serialized(self) -> Option<&'static str> {
        match self {
            Self::ToggleSidebar => Some("Command+B"),
            Self::OpenEnvironment => Some("Command+Shift+E"),
            Self::OpenAgents => Some("Command+Shift+A"),
            Self::OpenActivity => Some("Command+Shift+Y"),
            Self::OpenNotifications => Some("Command+Shift+U"),
            Self::NewShell => Some("Command+T"),
            Self::ClosePane => Some("Command+W"),
            Self::SplitVertical => Some("Command+D"),
            Self::SplitHorizontal => Some("Command+Shift+D"),
            Self::FocusNextPane => Some("Command+CloseBracket"),
            Self::FocusPreviousPane => Some("Command+OpenBracket"),
            Self::NextWorkspace => Some("Command+Alt+Right"),
            Self::PreviousWorkspace => Some("Command+Alt+Left"),
            Self::IncreaseTerminalFont => Some("Command+Plus"),
            Self::DecreaseTerminalFont => Some("Command+Minus"),
            // macOS Cmd+F 기본. 평문 Ctrl+F는 readline forward-char(C-f)와 충돌하므로
            // 기본으로 가로채지 않는다 — 사용자는 설정에서 Ctrl+F로 rebind할 수 있다 (T3).
            Self::TerminalSearch => Some("Command+F"),
            Self::ScrollToBottom => Some("Command+Down"),
            // egui Key::name()은 화살표를 "Up"/"Down"으로 직렬화한다 (⌘⇧↑/⌘⇧↓).
            Self::PromptJumpPrev => Some("Command+Shift+Up"),
            Self::PromptJumpNext => Some("Command+Shift+Down"),
            Self::FocusComposer => Some("Command+J"),
            Self::ClearRenderCaches => Some("Command+Alt+K"),
            Self::PreviousAgent
            | Self::NextAgent
            | Self::FocusAgentInput
            | Self::NewStructuredAgent
            | Self::InterruptAgent
            | Self::ApproveAgent
            | Self::RejectAgent
            | Self::IncreaseAgentEffort
            | Self::DecreaseAgentEffort => None,
        }
    }
}

pub fn effective_binding(
    config: &ShortcutsConfig,
    action: ShortcutAction,
) -> Option<egui::KeyboardShortcut> {
    if config.disabled.contains(action.id()) {
        return None;
    }
    config
        .bindings
        .get(action.id())
        .map(String::as_str)
        .and_then(parse_binding)
        .or_else(|| action.default_serialized().and_then(parse_binding))
}

pub fn set_binding(
    config: &mut ShortcutsConfig,
    action: ShortcutAction,
    binding: Option<egui::KeyboardShortcut>,
) {
    match binding {
        Some(binding) => {
            config.disabled.remove(action.id());
            let serialized = serialize_binding(binding);
            if action
                .default_serialized()
                .is_some_and(|default| serialized == default)
            {
                config.bindings.remove(action.id());
            } else {
                config.bindings.insert(action.id().to_owned(), serialized);
            }
        }
        None => {
            config.bindings.remove(action.id());
            config.disabled.insert(action.id().to_owned());
        }
    }
}

pub fn reset_binding(config: &mut ShortcutsConfig, action: ShortcutAction) {
    config.bindings.remove(action.id());
    config.disabled.remove(action.id());
}

pub fn reset_all(config: &mut ShortcutsConfig) {
    config.bindings.clear();
    config.disabled.clear();
}

pub fn conflicts(config: &ShortcutsConfig) -> BTreeSet<ShortcutAction> {
    let mut by_chord: BTreeMap<String, Vec<ShortcutAction>> = BTreeMap::new();
    for action in ShortcutAction::ALL {
        if let Some(binding) = effective_binding(config, action) {
            by_chord
                .entry(serialize_binding(binding))
                .or_default()
                .push(action);
        }
    }
    by_chord
        .into_values()
        .filter(|actions| actions.len() > 1)
        .flatten()
        .collect()
}

/// 메인 viewport에서 정확히 일치하는 key-down batch를 소비한다. 같은 물리 입력에서
/// repeat/중복 이벤트가 함께 와도 최초 non-repeat 한 건만 실행하고 전부 제거한다.
/// 중복 binding은 어느 동작도 실행하지 않아 예측 불가능한 다중 실행을 막는다.
pub fn take_triggered_action(
    ctx: &egui::Context,
    config: &ShortcutsConfig,
) -> Option<ShortcutAction> {
    let conflicts = conflicts(config);
    let bindings: Vec<_> = ShortcutAction::ALL
        .into_iter()
        .filter(|action| !conflicts.contains(action))
        .filter_map(|action| effective_binding(config, action).map(|binding| (action, binding)))
        .collect();
    ctx.input_mut(|input| take_triggered_action_from_events(&mut input.events, &bindings))
}

fn take_triggered_action_from_events(
    events: &mut Vec<egui::Event>,
    bindings: &[(ShortcutAction, egui::KeyboardShortcut)],
) -> Option<ShortcutAction> {
    let mut triggered = None;
    events.retain(|event| {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } = event
        else {
            return true;
        };
        let Some((action, _)) = bindings.iter().find(|(_, binding)| {
            binding.logical_key == *key && modifiers.matches_exact(binding.modifiers)
        }) else {
            return true;
        };
        if !repeat && triggered.is_none() {
            triggered = Some(*action);
        }
        false
    });
    triggered
}

/// 녹화 중 받은 key-down을 portable shortcut으로 정규화한다. 문자 입력을 가로채지 않도록
/// 일반 문자가 터미널 입력을 가로채지 않도록 Command/Ctrl/Alt 조합만 허용하되,
/// 외부 키패드가 안전하게 쓸 수 있는 F13~F24는 modifier 없이도 허용한다.
pub fn captured_binding(event: &egui::Event) -> Option<egui::KeyboardShortcut> {
    let egui::Event::Key {
        key,
        pressed: true,
        repeat: false,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    if matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    ) {
        return None;
    }
    let normalized = normalized_modifiers(*modifiers);
    binding_is_safe(*key, normalized).then(|| egui::KeyboardShortcut::new(normalized, *key))
}

fn normalized_modifiers(modifiers: egui::Modifiers) -> egui::Modifiers {
    egui::Modifiers {
        alt: modifiers.alt,
        ctrl: modifiers.ctrl,
        shift: modifiers.shift,
        mac_cmd: false,
        command: modifiers.command,
    }
}

pub fn serialize_binding(binding: egui::KeyboardShortcut) -> String {
    let mut parts = Vec::with_capacity(5);
    if binding.modifiers.command {
        parts.push("Command");
    }
    if binding.modifiers.ctrl {
        parts.push("Ctrl");
    }
    if binding.modifiers.alt {
        parts.push("Alt");
    }
    if binding.modifiers.shift {
        parts.push("Shift");
    }
    parts.push(binding.logical_key.name());
    parts.join("+")
}

pub fn parse_binding(value: &str) -> Option<egui::KeyboardShortcut> {
    let mut modifiers = egui::Modifiers::NONE;
    let mut key = None;
    for part in value.split('+').filter(|part| !part.is_empty()) {
        match part {
            "Command" => modifiers |= egui::Modifiers::COMMAND,
            "Ctrl" => modifiers |= egui::Modifiers::CTRL,
            "Alt" => modifiers |= egui::Modifiers::ALT,
            "Shift" => modifiers |= egui::Modifiers::SHIFT,
            name if key.is_none() => key = parse_key(name),
            _ => return None,
        }
    }
    let key = key?;
    binding_is_safe(key, modifiers).then(|| egui::KeyboardShortcut::new(modifiers, key))
}

fn binding_is_safe(key: egui::Key, modifiers: egui::Modifiers) -> bool {
    modifiers.command || modifiers.ctrl || modifiers.alt || is_extended_function_key(key)
}

fn is_extended_function_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::F13
            | egui::Key::F14
            | egui::Key::F15
            | egui::Key::F16
            | egui::Key::F17
            | egui::Key::F18
            | egui::Key::F19
            | egui::Key::F20
            | egui::Key::F21
            | egui::Key::F22
            | egui::Key::F23
            | egui::Key::F24
    )
}

fn parse_key(name: &str) -> Option<egui::Key> {
    egui::Key::from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_binding_roundtrip() {
        for action in ShortcutAction::ALL {
            if let Some(default) = action.default_serialized() {
                let binding = parse_binding(default).unwrap();
                assert_eq!(parse_binding(&serialize_binding(binding)), Some(binding));
            }
        }
    }

    #[test]
    fn duplicate_binding_marks_both_actions() {
        let mut config = ShortcutsConfig::default();
        let activity_binding = effective_binding(&config, ShortcutAction::OpenActivity);
        set_binding(&mut config, ShortcutAction::OpenAgents, activity_binding);
        let conflicts = conflicts(&config);
        assert!(conflicts.contains(&ShortcutAction::OpenAgents));
        assert!(conflicts.contains(&ShortcutAction::OpenActivity));
    }

    #[test]
    fn disabled_binding_survives_default_fallback() {
        let mut config = ShortcutsConfig::default();
        set_binding(&mut config, ShortcutAction::NewShell, None);
        assert_eq!(effective_binding(&config, ShortcutAction::NewShell), None);
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers,
        }
    }

    #[test]
    fn extended_function_keys_roundtrip_without_modifiers() {
        for key in [
            egui::Key::F13,
            egui::Key::F14,
            egui::Key::F15,
            egui::Key::F16,
            egui::Key::F17,
            egui::Key::F18,
            egui::Key::F19,
            egui::Key::F20,
            egui::Key::F21,
            egui::Key::F22,
            egui::Key::F23,
            egui::Key::F24,
        ] {
            let event = key_event(key, egui::Modifiers::NONE, false);
            let binding = captured_binding(&event).expect("F13~F24는 단독 허용");
            assert_eq!(binding.logical_key, key);
            assert_eq!(binding.modifiers, egui::Modifiers::NONE);
            assert_eq!(parse_binding(&serialize_binding(binding)), Some(binding));
        }
    }

    #[test]
    fn ordinary_and_standard_function_keys_still_require_modifiers() {
        for key in [egui::Key::A, egui::Key::Space, egui::Key::F12] {
            assert!(captured_binding(&key_event(key, egui::Modifiers::NONE, false)).is_none());
            assert!(parse_binding(key.name()).is_none());
        }
        assert!(
            captured_binding(&key_event(egui::Key::A, egui::Modifiers::COMMAND, false)).is_some()
        );
    }

    #[test]
    fn agent_actions_are_unassigned_by_default() {
        let config = ShortcutsConfig::default();
        for action in [
            ShortcutAction::PreviousAgent,
            ShortcutAction::NextAgent,
            ShortcutAction::FocusAgentInput,
            ShortcutAction::NewStructuredAgent,
            ShortcutAction::InterruptAgent,
            ShortcutAction::ApproveAgent,
            ShortcutAction::RejectAgent,
            ShortcutAction::IncreaseAgentEffort,
            ShortcutAction::DecreaseAgentEffort,
        ] {
            assert_eq!(effective_binding(&config, action), None);
        }
    }

    #[test]
    fn matching_repeat_and_duplicate_events_are_all_consumed_once() {
        let binding = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::F13);
        let bindings = [(ShortcutAction::InterruptAgent, binding)];
        let mut events = vec![
            key_event(egui::Key::F13, egui::Modifiers::NONE, false),
            key_event(egui::Key::F13, egui::Modifiers::NONE, true),
            key_event(egui::Key::F13, egui::Modifiers::NONE, false),
            egui::Event::Text("kept".to_owned()),
        ];
        assert_eq!(
            take_triggered_action_from_events(&mut events, &bindings),
            Some(ShortcutAction::InterruptAgent)
        );
        assert_eq!(events, vec![egui::Event::Text("kept".to_owned())]);
    }

    #[test]
    fn repeat_only_event_is_consumed_without_triggering_again() {
        let binding = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::F13);
        let bindings = [(ShortcutAction::InterruptAgent, binding)];
        let mut events = vec![key_event(egui::Key::F13, egui::Modifiers::NONE, true)];
        assert_eq!(
            take_triggered_action_from_events(&mut events, &bindings),
            None
        );
        assert!(events.is_empty());
    }
}
