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
    NextAgentModel,
    PreviousAgentModel,
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
    pub const ALL: [Self; 32] = [
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
        Self::NextAgentModel,
        Self::PreviousAgentModel,
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
            Self::NextAgentModel => "next_agent_model",
            Self::PreviousAgentModel => "previous_agent_model",
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
            Self::NextAgentModel => "shortcuts.action.next_agent_model",
            Self::PreviousAgentModel => "shortcuts.action.previous_agent_model",
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
            | Self::DecreaseAgentEffort
            | Self::NextAgentModel
            | Self::PreviousAgentModel => ShortcutGroup::Agent,
        }
    }

    /// 두 플랫폼이 같은 기본값을 쓰는 액션.
    ///
    /// `Command`는 egui가 macOS에서 ⌘, 그 외에서 Ctrl로 푸는 논리 수정자다. 즉 여기
    /// 적는 `Command+X`는 Windows에서 **평문 Ctrl+X**가 된다 — 터미널 제어문자와
    /// 겹치는 글자는 이쪽에 두면 안 되고 `windows_default_serialized`로 분리한다.
    fn shared_default_serialized(self) -> Option<&'static str> {
        match self {
            Self::OpenEnvironment => Some("Command+Shift+E"),
            Self::OpenAgents => Some("Command+Shift+A"),
            Self::OpenActivity => Some("Command+Shift+Y"),
            Self::OpenNotifications => Some("Command+Shift+U"),
            // 두 플랫폼 통일 — Windows에서 SplitVertical이 Ctrl+Shift+D를 가져가므로
            // ⌘⇧D를 비우고 H(horizontal)로 옮겼다.
            Self::SplitHorizontal => Some("Command+Shift+H"),
            // ⌘+ 는 US 배열에서 Shift+= 라 egui가 `Key::Plus`+shift:true로 준다.
            // matches_exact는 shift까지 정확히 요구하므로 "Command+Plus"는 전용 + 키
            // (numpad)에서만 먹었다. Chrome/VS Code와 같은 `=` 기본으로 바꾼다 —
            // ⌘⇧= 를 쓰고 싶으면 설정에서 녹화하면 "Command+Shift+Plus"로 저장된다.
            Self::IncreaseTerminalFont => Some("Command+Equals"),
            Self::DecreaseTerminalFont => Some("Command+Minus"),
            Self::ScrollToBottom => Some("Command+Down"),
            // 에이전트 강도 다이얼. 여기만 논리 Command가 아니라 **실제 Ctrl**이다 —
            // macOS에서 ⌃⇧↑↓, Windows에서 Ctrl+Shift+↑↓ 로 물리 제스처가 같아진다.
            // macOS ⌃↑/⌃↓ 는 Mission Control이지만 Shift가 붙으면 시스템 예약이 아니다.
            Self::IncreaseAgentEffort => Some("Ctrl+Shift+Up"),
            Self::DecreaseAgentEffort => Some("Ctrl+Shift+Down"),
            // 강도가 세로축이면 모델은 가로축이다 — 같은 손 모양에서 방향만 바뀐다.
            Self::NextAgentModel => Some("Ctrl+Shift+Right"),
            Self::PreviousAgentModel => Some("Ctrl+Shift+Left"),
            Self::PreviousAgent => Some("Ctrl+Shift+OpenBracket"),
            Self::NextAgent => Some("Ctrl+Shift+CloseBracket"),
            Self::FocusAgentInput => Some("Command+Shift+I"),
            Self::NewStructuredAgent => Some("Command+Shift+N"),
            // ⌘. 은 macOS 고전 "취소". Windows Ctrl+. 도 비어 있다.
            Self::InterruptAgent => Some("Command+Period"),
            Self::ApproveAgent => Some("Command+Shift+Enter"),
            Self::RejectAgent => Some("Command+Shift+Backspace"),
            _ => None,
        }
    }

    /// Windows 전용 기본값. 두 부류를 피한다.
    ///
    /// 1. **터미널 제어문자** — `Command+D`는 Windows에서 Ctrl+D(EOF)라 셸을 끝내고,
    ///    `Command+OpenBracket`은 Ctrl+\[(ESC)라 vim 삽입모드 탈출을 먹는다. 앱은
    ///    터미널 포커스 중에도 가로채므로(`handle_configured_shortcut`의 게이트는
    ///    `text_edit_focused`뿐이고 터미널 pane은 TextEdit이 아니다) 회피가 유일한 답이다.
    ///    Windows Terminal 관례인 `Ctrl+Shift+*`로 옮긴다.
    /// 2. **Ctrl+Alt** — 독일어/북유럽/폴란드어 배열에서 AltGr이 Ctrl+Alt로 들어와
    ///    문자 입력 중 오발동한다. 또 Ctrl+Alt+화살표는 Intel/AMD 드라이버의 화면 회전
    ///    핫키라 앱에 도달조차 하지 않는다.
    ///
    /// 두 표 모두 **항상 컴파일**한다. `#[cfg(windows)]`로 잘라두면 macOS에서 Windows
    /// 기본값을 검증할 방법이 없어, Ctrl+Shift+D 중복 같은 사고가 Windows 빌드에
    /// 도달해서야 드러난다. `cfg!`는 어느 표를 쓸지만 고른다.
    fn windows_default_serialized(self) -> Option<&'static str> {
        match self {
            Self::ToggleSidebar => Some("Ctrl+Shift+B"), // Ctrl+B: backward-char, tmux prefix
            Self::NewShell => Some("Ctrl+Shift+T"),      // Ctrl+T: transpose-chars
            Self::ClosePane => Some("Ctrl+Shift+W"),     // Ctrl+W: kill-word
            Self::SplitVertical => Some("Ctrl+Shift+D"), // Ctrl+D: EOF
            Self::TerminalSearch => Some("Ctrl+Shift+F"), // Ctrl+F: forward-char
            Self::FocusComposer => Some("Ctrl+Shift+J"), // Ctrl+J: LF
            // Ctrl+[ = ESC, Ctrl+] = GS. Ctrl+Shift+대괄호는 에이전트 선택이 쓰므로
            // pane 이동은 WezTerm/Windows Terminal 관례인 Alt+화살표로 간다.
            Self::FocusNextPane => Some("Alt+Right"),
            Self::FocusPreviousPane => Some("Alt+Left"),
            // Ctrl+Alt+화살표 = 드라이버 화면 회전. Ctrl+PageUp/Down은 Windows 표준
            // "이전/다음 탭"이라 워크스페이스 전환에 그대로 맞는다.
            Self::NextWorkspace => Some("Ctrl+PageDown"),
            Self::PreviousWorkspace => Some("Ctrl+PageUp"),
            // Ctrl+Shift+↑↓ 를 에이전트 강도에 내주고 프롬프트 점프가 비킨다.
            // Alt+화살표와 달리 Alt+↑↓ 는 pane 이동과 겹치지 않는다.
            Self::PromptJumpPrev => Some("Alt+Up"),
            Self::PromptJumpNext => Some("Alt+Down"),
            Self::ClearRenderCaches => Some("Ctrl+Shift+K"), // Ctrl+Alt+K: AltGr
            _ => self.shared_default_serialized(),
        }
    }

    fn unix_default_serialized(self) -> Option<&'static str> {
        match self {
            Self::ToggleSidebar => Some("Command+B"),
            Self::NewShell => Some("Command+T"),
            Self::ClosePane => Some("Command+W"),
            Self::SplitVertical => Some("Command+D"),
            // macOS Cmd+F 기본. 평문 Ctrl+F는 readline forward-char(C-f)와 충돌하므로
            // 기본으로 가로채지 않는다 — 사용자는 설정에서 Ctrl+F로 rebind할 수 있다 (T3).
            Self::TerminalSearch => Some("Command+F"),
            Self::FocusComposer => Some("Command+J"),
            Self::FocusNextPane => Some("Command+CloseBracket"),
            Self::FocusPreviousPane => Some("Command+OpenBracket"),
            Self::NextWorkspace => Some("Command+Alt+Right"),
            Self::PreviousWorkspace => Some("Command+Alt+Left"),
            // egui Key::name()은 화살표를 "Up"/"Down"으로 직렬화한다 (⌘⇧↑/⌘⇧↓).
            Self::PromptJumpPrev => Some("Command+Shift+Up"),
            Self::PromptJumpNext => Some("Command+Shift+Down"),
            Self::ClearRenderCaches => Some("Command+Alt+K"),
            _ => self.shared_default_serialized(),
        }
    }

    fn default_serialized(self) -> Option<&'static str> {
        if cfg!(windows) {
            self.windows_default_serialized()
        } else {
            self.unix_default_serialized()
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

/// 같은 물리 입력이 두 binding에 모두 매칭되는지 판정할 때 쓰는 키.
///
/// `serialize_binding`을 그대로 쓰면 안 된다. macOS 밖에서는 winit이 Ctrl을 누를 때
/// `ctrl`과 `command`를 **둘 다** 세우고, egui `cmd_ctrl_matches`는 `Command+X`와
/// `Ctrl+X` 양쪽 패턴을 모두 통과시킨다. 직렬화 문자열은 다르지만 실제로는 같은 키다 —
/// 이 경우 `take_triggered_action_from_events`의 `find`가 `ShortcutAction::ALL` 순서상
/// 앞선 액션만 실행하고 나머지는 조용히 죽는다. 그래서 mac이 아닌 곳에서는 두 수정자를
/// 한 비트로 접어서 중복으로 잡는다. macOS에서는 ⌘와 ⌃가 실제로 다른 키라 구분한다.
fn conflict_key_on(binding: egui::KeyboardShortcut, macos: bool) -> String {
    let m = binding.modifiers;
    let cmd_ctrl = if macos {
        match (m.command, m.ctrl) {
            (true, true) => "cmd+ctrl",
            (true, false) => "cmd",
            (false, true) => "ctrl",
            (false, false) => "",
        }
    } else if m.command || m.ctrl {
        "cmdctrl"
    } else {
        ""
    };
    format!(
        "{cmd_ctrl}|{}|{}|{}",
        m.alt,
        m.shift,
        binding.logical_key.name()
    )
}

fn conflict_key(binding: egui::KeyboardShortcut) -> String {
    conflict_key_on(binding, cfg!(target_os = "macos"))
}

pub fn conflicts(config: &ShortcutsConfig) -> BTreeSet<ShortcutAction> {
    let mut by_chord: BTreeMap<String, Vec<ShortcutAction>> = BTreeMap::new();
    for action in ShortcutAction::ALL {
        if let Some(binding) = effective_binding(config, action) {
            by_chord
                .entry(conflict_key(binding))
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

    /// (표 이름, 조회 함수, 그 표가 macOS 규칙을 쓰는지)
    ///
    /// 두 표를 **어느 플랫폼에서 돌려도** 검사한다. macOS에서만 테스트를 돌리면서
    /// Windows 표를 cfg로 잘라두면, Ctrl+Shift+D 중복 같은 사고가 Windows 사용자에게
    /// 가서야 드러난다 — 실제로 SplitVertical(Ctrl+Shift+D)이 기존
    /// SplitHorizontal(⌘⇧D → Windows에서 Ctrl+Shift+D)과 부딪혀 H로 옮겼다.
    const DEFAULT_TABLES: [(&str, fn(ShortcutAction) -> Option<&'static str>, bool); 2] = [
        ("unix", ShortcutAction::unix_default_serialized, true),
        ("windows", ShortcutAction::windows_default_serialized, false),
    ];

    /// 기본값끼리 겹치면 `conflicts()`가 **양쪽 다** 비활성화해서 두 동작이 통째로
    /// 사라진다. 조용한 실패라 테스트로만 잡힌다.
    #[test]
    fn 두_플랫폼_기본값_모두_중복이_없다() {
        for (name, table, macos) in DEFAULT_TABLES {
            let mut by_chord: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for action in ShortcutAction::ALL {
                let Some(default) = table(action) else {
                    continue;
                };
                let binding = parse_binding(default)
                    .unwrap_or_else(|| panic!("{name}: {}의 {default}를 파싱 실패", action.id()));
                by_chord
                    .entry(conflict_key_on(binding, macos))
                    .or_default()
                    .push(action.id());
            }
            let dupes: Vec<_> = by_chord
                .iter()
                .filter(|(_, actions)| actions.len() > 1)
                .collect();
            assert!(dupes.is_empty(), "{name} 기본값 충돌: {dupes:?}");
        }
    }

    /// 모든 기본값이 파싱돼야 한다. `binding_is_safe`가 거르면 `parse_binding`이 None을
    /// 주고 그 액션은 조용히 바인딩 없이 출시된다.
    #[test]
    fn 두_플랫폼_기본값_모두_파싱된다() {
        for (name, table, _) in DEFAULT_TABLES {
            for action in ShortcutAction::ALL {
                let Some(default) = table(action) else {
                    continue;
                };
                let binding = parse_binding(default)
                    .unwrap_or_else(|| panic!("{name}: {}의 {default} 파싱 실패", action.id()));
                assert_eq!(
                    parse_binding(&serialize_binding(binding)),
                    Some(binding),
                    "{name}: {}의 {default}가 왕복하지 않는다",
                    action.id()
                );
            }
        }
    }

    /// 두 표는 같은 액션 집합을 덮어야 한다 — 한쪽에만 기본값이 있으면 그 플랫폼
    /// 사용자만 조용히 단축키 없이 쓰게 된다.
    #[test]
    fn 두_플랫폼_기본값_집합이_같다() {
        for action in ShortcutAction::ALL {
            assert_eq!(
                action.unix_default_serialized().is_some(),
                action.windows_default_serialized().is_some(),
                "{}의 기본값 유무가 플랫폼마다 다르다",
                action.id()
            );
        }
    }

    /// 에이전트 그룹은 전부 기본 바인딩을 가진다 — 예약해 둔 모델 전환 2종만 예외이며
    /// 그건 아직 액션 자체가 없다.
    #[test]
    fn 에이전트_액션에_기본값이_있다() {
        for action in ShortcutAction::ALL {
            if action.group() != ShortcutGroup::Agent {
                continue;
            }
            assert!(
                action.default_serialized().is_some(),
                "{}에 기본 바인딩이 없다",
                action.id()
            );
        }
    }

    /// Windows 기본값에 Ctrl+Alt가 있으면 두 가지로 깨진다: AltGr(Ctrl+Alt) 배열에서
    /// 문자 입력 중 오발동하고, 화살표 조합은 Intel/AMD 드라이버 화면 회전 핫키가
    /// 먼저 먹어 앱에 도달하지 않는다.
    #[test]
    fn windows_기본값은_ctrl_alt를_쓰지_않는다() {
        for action in ShortcutAction::ALL {
            let Some(binding) = action.windows_default_serialized().and_then(parse_binding) else {
                continue;
            };
            let cmd_or_ctrl = binding.modifiers.command || binding.modifiers.ctrl;
            assert!(
                !(cmd_or_ctrl && binding.modifiers.alt),
                "{}의 Windows 기본값이 Ctrl+Alt다 (AltGr/화면회전 충돌)",
                action.id()
            );
        }
    }

    /// Windows에서 앱이 가로채면 셸에 도달하지 못하는 제어문자 조합. 터미널 pane은
    /// TextEdit이 아니라 `handle_configured_shortcut`의 포커스 게이트를 통과하므로,
    /// 기본값 단계에서 피하는 것 말고 방법이 없다.
    #[test]
    fn windows_기본값은_터미널_제어문자를_가로채지_않는다() {
        // (키, 무엇을 먹는지)
        let reserved = [
            (egui::Key::D, "EOF"),
            (egui::Key::OpenBracket, "ESC"),
            (egui::Key::CloseBracket, "GS"),
            (egui::Key::B, "backward-char/tmux prefix"),
            (egui::Key::T, "transpose-chars"),
            (egui::Key::W, "kill-word"),
            (egui::Key::F, "forward-char"),
            (egui::Key::J, "LF"),
            (egui::Key::C, "SIGINT"),
            (egui::Key::Z, "SIGTSTP"),
            (egui::Key::U, "kill-line"),
        ];
        for action in ShortcutAction::ALL {
            let Some(binding) = action.windows_default_serialized().and_then(parse_binding) else {
                continue;
            };
            let m = binding.modifiers;
            // Shift가 붙으면 제어문자가 아니라 별개 chord다 (Ctrl+Shift+D ≠ Ctrl+D).
            if !(m.command || m.ctrl) || m.shift || m.alt {
                continue;
            }
            if let Some((_, meaning)) = reserved.iter().find(|(key, _)| *key == binding.logical_key)
            {
                panic!(
                    "{}의 Windows 기본값이 Ctrl+{:?} — 셸의 {meaning}를 먹는다",
                    action.id(),
                    binding.logical_key
                );
            }
        }
    }

    /// macOS가 아닌 곳에서 winit은 Ctrl 하나에 `ctrl`과 `command`를 둘 다 세운다.
    /// 그래서 `Command+B`와 `Ctrl+B`는 직렬화 문자열만 다를 뿐 같은 물리 키다.
    /// `conflict_key`가 이걸 접지 않으면 둘 중 하나가 조용히 죽는다.
    #[test]
    fn command와_ctrl은_mac_밖에서_같은_키로_취급된다() {
        let command_b = parse_binding("Command+B").unwrap();
        let ctrl_b = parse_binding("Ctrl+B").unwrap();
        assert_ne!(command_b, ctrl_b, "두 chord는 서로 다른 값이어야 한다");
        if cfg!(target_os = "macos") {
            assert_ne!(conflict_key(command_b), conflict_key(ctrl_b));
        } else {
            assert_eq!(conflict_key(command_b), conflict_key(ctrl_b));
        }
    }

    /// 위 규칙이 `conflicts()`까지 실제로 전달되는지 — 단위 함수만 맞고 호출부가
    /// 옛 키를 쓰면 의미가 없다.
    #[test]
    fn mac_밖에서는_command와_ctrl_바인딩이_충돌로_잡힌다() {
        let mut config = ShortcutsConfig::default();
        set_binding(
            &mut config,
            ShortcutAction::ToggleSidebar,
            parse_binding("Command+Shift+Y"),
        );
        set_binding(
            &mut config,
            ShortcutAction::OpenActivity,
            parse_binding("Ctrl+Shift+Y"),
        );
        let conflicts = conflicts(&config);
        if cfg!(target_os = "macos") {
            assert!(conflicts.is_empty(), "macOS에서 ⌘와 ⌃는 다른 키다");
        } else {
            assert!(conflicts.contains(&ShortcutAction::ToggleSidebar));
            assert!(conflicts.contains(&ShortcutAction::OpenActivity));
        }
    }

    /// ⌘+ 는 US 배열에서 Shift+= 라 egui가 `Key::Plus` + `shift: true`로 준다.
    /// `matches_exact`는 shift까지 정확히 요구하므로 옛 기본값 "Command+Plus"는
    /// 전용 + 키에서만 먹었다. 새 기본값은 shift 없이 눌리는 `=` 여야 한다.
    #[test]
    fn 글꼴_확대_기본값은_shift_없이_눌린다() {
        let binding = ShortcutAction::IncreaseTerminalFont
            .default_serialized()
            .and_then(parse_binding)
            .expect("기본값이 있어야 한다");
        assert!(!binding.modifiers.shift);
        assert_eq!(binding.logical_key, egui::Key::Equals);
        // 실제 입력 경로 재현: '=' 를 shift 없이 누르면 매칭돼야 한다.
        assert!(
            egui::Modifiers::COMMAND.matches_exact(binding.modifiers),
            "⌘= 가 매칭되지 않는다"
        );
        // 옛 기본값이 왜 안 먹었는지 고정 — Shift+= 는 Plus+shift로 온다.
        let old = parse_binding("Command+Plus").unwrap();
        let shift_equals = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        assert!(
            !shift_equals.matches_exact(old.modifiers),
            "옛 기본값이 갑자기 매칭되면 이 회귀 테스트의 전제가 바뀐 것이다"
        );
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

    /// 에이전트 액션은 원래 전부 미할당이었다(사용자가 설정에서 직접 걸어야 했다).
    /// 이제 기본값을 준다 — 미할당이 아니라 "해제하면 없어진다"가 계약이다.
    #[test]
    fn agent_actions_are_bound_by_default_and_stay_unbindable() {
        let mut config = ShortcutsConfig::default();
        let agent_actions = [
            ShortcutAction::PreviousAgent,
            ShortcutAction::NextAgent,
            ShortcutAction::FocusAgentInput,
            ShortcutAction::NewStructuredAgent,
            ShortcutAction::InterruptAgent,
            ShortcutAction::ApproveAgent,
            ShortcutAction::RejectAgent,
            ShortcutAction::IncreaseAgentEffort,
            ShortcutAction::DecreaseAgentEffort,
        ];
        for action in agent_actions {
            assert!(
                effective_binding(&config, action).is_some(),
                "{}에 기본 바인딩이 없다",
                action.id()
            );
        }
        for action in agent_actions {
            set_binding(&mut config, action, None);
            assert_eq!(effective_binding(&config, action), None);
        }
    }

    /// 사용자가 요청한 강도 다이얼이 실제로 그 chord로 나가는지 고정한다.
    /// macOS ⌃⇧↑↓ / Windows Ctrl+Shift+↑↓ — 물리 제스처가 같아야 한다.
    #[test]
    fn 강도_다이얼은_양_플랫폼에서_같은_제스처다() {
        let up = ShortcutAction::IncreaseAgentEffort
            .default_serialized()
            .and_then(parse_binding)
            .unwrap();
        let down = ShortcutAction::DecreaseAgentEffort
            .default_serialized()
            .and_then(parse_binding)
            .unwrap();
        assert_eq!(up.logical_key, egui::Key::ArrowUp);
        assert_eq!(down.logical_key, egui::Key::ArrowDown);
        for binding in [up, down] {
            assert!(
                binding.modifiers.ctrl,
                "논리 Command가 아니라 실제 Ctrl이어야 한다"
            );
            assert!(binding.modifiers.shift);
            assert!(!binding.modifiers.alt);
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
