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
    ClearRenderCaches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutGroup {
    Navigation,
    Terminal,
    Workspace,
}

impl ShortcutGroup {
    pub const ALL: [Self; 3] = [Self::Navigation, Self::Terminal, Self::Workspace];

    pub const fn title_key(self) -> &'static str {
        match self {
            Self::Navigation => "shortcuts.group.navigation",
            Self::Terminal => "shortcuts.group.terminal",
            Self::Workspace => "shortcuts.group.workspace",
        }
    }
}

impl ShortcutAction {
    pub const ALL: [Self; 16] = [
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
        Self::ClearRenderCaches,
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
            Self::ClearRenderCaches => "clear_render_caches",
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
            Self::ClearRenderCaches => "shortcuts.action.clear_render_caches",
        }
    }

    pub const fn description_key(self) -> &'static str {
        match self.group() {
            ShortcutGroup::Navigation => "shortcuts.desc.navigation",
            ShortcutGroup::Terminal => "shortcuts.desc.terminal",
            ShortcutGroup::Workspace => "shortcuts.desc.workspace",
        }
    }

    pub const fn group(self) -> ShortcutGroup {
        match self {
            Self::ToggleSidebar
            | Self::OpenEnvironment
            | Self::OpenAgents
            | Self::OpenActivity
            | Self::OpenNotifications => ShortcutGroup::Navigation,
            Self::NewShell
            | Self::ClosePane
            | Self::SplitVertical
            | Self::SplitHorizontal
            | Self::FocusNextPane
            | Self::FocusPreviousPane
            | Self::IncreaseTerminalFont
            | Self::DecreaseTerminalFont => ShortcutGroup::Terminal,
            Self::NextWorkspace | Self::PreviousWorkspace | Self::ClearRenderCaches => {
                ShortcutGroup::Workspace
            }
        }
    }

    fn default_serialized(self) -> &'static str {
        match self {
            Self::ToggleSidebar => "Command+B",
            Self::OpenEnvironment => "Command+Shift+E",
            Self::OpenAgents => "Command+Shift+A",
            Self::OpenActivity => "Command+Shift+Y",
            Self::OpenNotifications => "Command+Shift+U",
            Self::NewShell => "Command+T",
            Self::ClosePane => "Command+W",
            Self::SplitVertical => "Command+D",
            Self::SplitHorizontal => "Command+Shift+D",
            Self::FocusNextPane => "Command+CloseBracket",
            Self::FocusPreviousPane => "Command+OpenBracket",
            Self::NextWorkspace => "Command+Alt+Right",
            Self::PreviousWorkspace => "Command+Alt+Left",
            Self::IncreaseTerminalFont => "Command+Plus",
            Self::DecreaseTerminalFont => "Command+Minus",
            Self::ClearRenderCaches => "Command+Alt+K",
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
        .or_else(|| parse_binding(action.default_serialized()))
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
            if serialized == action.default_serialized() {
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

/// 메인 viewport에서 정확히 일치하는 key-down 이벤트 한 건을 소비한다. 중복 binding은
/// 어느 동작도 실행하지 않아 예측 불가능한 다중 실행을 막는다.
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
    ctx.input_mut(|input| {
        let found = input.events.iter().enumerate().find_map(|(index, event)| {
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
            bindings
                .iter()
                .find(|(_, binding)| {
                    binding.logical_key == *key && modifiers.matches_exact(binding.modifiers)
                })
                .map(|(action, _)| (index, *action))
        });
        found.map(|(index, action)| {
            input.events.remove(index);
            action
        })
    })
}

/// 녹화 중 받은 key-down을 portable shortcut으로 정규화한다. 문자 입력을 가로채지 않도록
/// Command/Ctrl/Alt 중 하나가 없는 단일 키는 등록하지 않는다.
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
    (normalized.command || normalized.ctrl || normalized.alt)
        .then(|| egui::KeyboardShortcut::new(normalized, *key))
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
    (modifiers.command || modifiers.ctrl || modifiers.alt)
        .then(|| egui::KeyboardShortcut::new(modifiers, key))
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
            let binding = parse_binding(action.default_serialized()).unwrap();
            assert_eq!(parse_binding(&serialize_binding(binding)), Some(binding));
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
}
