//! Local permission held through the actual PTY queue write, never across event wakes.
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct InputPermit(Arc<Mutex<bool>>);

impl InputPermit {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(true)))
    }

    /// After this returns, this permit can admit no further input.
    pub fn revoke(&self) {
        if let Ok(mut allowed) = self.0.lock() {
            *allowed = false;
        }
    }
}

impl Default for InputPermit {
    fn default() -> Self {
        Self::new()
    }
}

/// Local-only evidence. This is not sent over the runtime wire.
#[derive(Debug, Clone, Copy)]
pub struct AgentInputGuard {
    pub foreground_process_group: u32,
    pub provider: AgentPromptKind,
    pub intent: AgentInputIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentInputIntent {
    AutomaticPrompt,
    ExplicitPrompt,
    /// Deliberate no-submit append; existing draft text is expected and preserved.
    ExplicitAppend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPromptKind {
    Claude,
    Codex,
    Other,
}

pub struct InputAdmission {
    agent_guard: Option<AgentInputGuard>,
    require_bracketed_paste: bool,
    permit: InputPermit,
    deadline: Instant,
    authorize: Box<InputAuthorizer>,
}

type InputAuthorizer = dyn Fn(&mut dyn FnMut()) + Send + Sync;

impl InputAdmission {
    pub fn new(
        permit: InputPermit,
        deadline: Instant,
        authorize: impl Fn(&mut dyn FnMut()) + Send + Sync + 'static,
    ) -> Self {
        Self {
            agent_guard: None,
            require_bracketed_paste: false,
            permit,
            deadline,
            authorize: Box::new(authorize),
        }
    }

    pub fn with_agent_guard(mut self, guard: AgentInputGuard) -> Self {
        self.agent_guard = Some(guard);
        self
    }

    /// Local-only requirement for encoded paste; rechecked at actual PTY queue retention.
    pub fn with_bracketed_paste_required(mut self) -> Self {
        self.require_bracketed_paste = true;
        self
    }

    pub(crate) fn requires_bracketed_paste(&self) -> bool {
        self.require_bracketed_paste
    }

    pub(crate) fn agent_guard(&self) -> Option<AgentInputGuard> {
        self.agent_guard
    }

    pub(crate) fn deadline_elapsed(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn admit<T>(&self, write: impl FnOnce() -> T) -> Option<T> {
        let allowed = self.permit.0.lock().ok()?;
        if !*allowed || Instant::now() >= self.deadline {
            return None;
        }
        let mut write = Some(write);
        let mut result = None;
        (self.authorize)(&mut || {
            // The authorization callback may have waited for its own lock.
            if Instant::now() < self.deadline
                && let Some(write) = write.take()
            {
                result = Some(write());
            }
        });
        drop(allowed);
        result
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PromptRow {
    Empty,
    Draft,
    /// Native input hint; unlike an arbitrary unknown row, no user text is present.
    Placeholder,
    Unknown,
}

fn prompt_row(
    snapshot: &terminal::TerminalViewportSnapshot,
    provider: AgentPromptKind,
) -> PromptRow {
    if snapshot.scroll_offset != 0 || !snapshot.cursor.visible || snapshot.cols == 0 {
        return PromptRow::Unknown;
    }
    let marker = match provider {
        AgentPromptKind::Claude => '❯',
        AgentPromptKind::Codex => '›',
        AgentPromptKind::Other => return PromptRow::Unknown,
    };
    let start = usize::from(snapshot.cursor.row) * usize::from(snapshot.cols);
    let Some(cells) = snapshot
        .visible_cells
        .get(start..start + usize::from(snapshot.cols))
    else {
        return PromptRow::Unknown;
    };
    let Some(index) = cells.iter().position(|cell| !cell.c.is_whitespace()) else {
        return PromptRow::Unknown;
    };
    if cells[index].c != marker {
        return PromptRow::Unknown;
    }
    let tail = &cells[index + 1..];
    if tail.iter().all(|cell| cell.c.is_whitespace()) {
        return PromptRow::Empty;
    }
    // A dim suggestion before the cursor has entered it is not a verified empty prompt.
    // Keep that positive native-input evidence separate from an arbitrary unknown row.
    // Explicit visible-session sends can proceed; automation must still fail closed.
    let first = index
        + 1
        + tail
            .iter()
            .position(|cell| !cell.c.is_whitespace())
            .unwrap();
    if usize::from(snapshot.cursor.col) <= first
        && cells[first..]
            .iter()
            .filter(|cell| !cell.c.is_whitespace())
            .all(|cell| cell.attrs().contains(terminal::CellAttrs::DIM))
    {
        return PromptRow::Placeholder;
    }
    PromptRow::Draft
}

fn choice_dialog(snapshot: &terminal::TerminalViewportSnapshot) -> bool {
    // Bounded current viewport only, never scrollback or transcript polling.
    let first_row = usize::from(snapshot.cursor.row).saturating_sub(8);
    let last_row = (usize::from(snapshot.cursor.row) + 3).min(usize::from(snapshot.rows));
    let first = first_row * usize::from(snapshot.cols);
    let last = last_row * usize::from(snapshot.cols);
    let Some(cells) = snapshot.visible_cells.get(first..last) else {
        return true;
    };
    let text: String = cells.iter().map(|cell| cell.c).collect();
    let text = text.to_ascii_lowercase();
    text.contains("enter to select")
        || text.contains("press enter to confirm")
        || text.contains("esc to cancel")
        || text.contains("escape to cancel")
}

impl AgentInputGuard {
    fn deny(self, reason: &'static str) -> bool {
        // Diagnostic tags only: never log a prompt, terminal cells or user content.
        tracing::info!(component = "agent_input", reason, intent = ?self.intent,
            provider = ?self.provider, "input admission declined");
        false
    }

    pub(crate) fn allows(
        self,
        active: &session::Session,
        detector: Option<&session::StatusDetector>,
    ) -> bool {
        if active.foreground_process_group() != Some(self.foreground_process_group) {
            return self.deny("foreground_changed");
        }
        let Some(detector) = detector else {
            return self.deny("detector_unavailable");
        };
        let snapshot = active.input_guard_snapshot();
        if snapshot.as_ref().is_some_and(choice_dialog) {
            return self.deny("current_choice_dialog");
        }
        let row = snapshot.as_ref().map_or(PromptRow::Unknown, |snapshot| {
            prompt_row(snapshot, self.provider)
        });
        if self.intent != AgentInputIntent::ExplicitAppend
            && detector.has_input_draft()
            && !(self.intent == AgentInputIntent::ExplicitPrompt
                && row == PromptRow::Empty
                && detector.has_redrawn_erase_attempt()
                && !detector.has_screen_input_request())
        {
            return self.deny("accepted_input_draft");
        }
        // Stream status is latched until input. It cannot veto that very input after
        // the CLI has visibly returned to its native editor. This exception is only
        // for deliberate submit with positive ready-row evidence; current screen
        // approval state, dialogs, accepted drafts and automatic sends still veto.
        if self.intent == AgentInputIntent::ExplicitPrompt
            && matches!(row, PromptRow::Empty | PromptRow::Placeholder)
            && detector.status_view(None).source == session::StatusSource::StreamRegex
            && !detector.has_screen_input_request()
        {
            return true;
        }
        if matches!(
            detector.status(),
            session::SessionStatus::Waiting | session::SessionStatus::NeedsApproval
        ) {
            return self.deny("current_input_request");
        }
        match row {
            PromptRow::Empty => true,
            PromptRow::Draft if self.intent == AgentInputIntent::ExplicitAppend => true,
            PromptRow::Draft => self.deny("visible_input_draft"),
            PromptRow::Placeholder | PromptRow::Unknown
                if self.intent != AgentInputIntent::AutomaticPrompt =>
            {
                true
            }
            PromptRow::Placeholder | PromptRow::Unknown => {
                self.deny("automatic_prompt_not_verified_empty")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(ansi: &str) -> terminal::TerminalViewportSnapshot {
        let live = session::Session::restore_archived(
            crate::SessionId(1),
            session::SessionKind::Agent,
            80,
            24,
            100,
            Some(0),
            &mut ansi.as_bytes(),
        );
        live.input_guard_snapshot().unwrap()
    }

    #[test]
    fn pr2_supported_cursor_row_requires_empty_prompt_for_automation() {
        assert_eq!(
            prompt_row(&snapshot("❯ "), AgentPromptKind::Claude),
            PromptRow::Empty
        );
        assert_eq!(
            prompt_row(&snapshot("› "), AgentPromptKind::Codex),
            PromptRow::Empty
        );
        assert_eq!(
            prompt_row(&snapshot("❯ existing draft"), AgentPromptKind::Claude),
            PromptRow::Draft
        );
        assert_eq!(
            prompt_row(&snapshot("› existing draft"), AgentPromptKind::Codex),
            PromptRow::Draft
        );
        assert_eq!(
            prompt_row(&snapshot("❯ "), AgentPromptKind::Other),
            PromptRow::Unknown
        );
        assert_eq!(
            prompt_row(&snapshot("shell$ "), AgentPromptKind::Claude),
            PromptRow::Unknown
        );
        assert_eq!(
            prompt_row(&snapshot("❯ \x1b[?25l"), AgentPromptKind::Claude),
            PromptRow::Unknown
        );
        assert!(choice_dialog(&snapshot("❯ Yes, allow\r\nEnter to select")));
        assert!(choice_dialog(&snapshot(
            "Press enter to confirm or esc to cancel"
        )));
    }
}
