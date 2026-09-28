//! Deterministic, full UI-frame terminal input traces. This drives egui's raw
//! events through the real workspace renderer and checks session-owned PTY writes.
use super::*;

struct TerminalInputHarness {
    ui: egui_kittest::Harness<'static, WorkspaceUi>,
}

impl TerminalInputHarness {
    fn new(session: SessionId) -> Self {
        Self {
            ui: setup_focused_local_pane_harness(session),
        }
    }

    fn frame(
        &mut self,
        events: impl IntoIterator<Item = egui::Event>,
    ) -> Vec<(SessionId, Vec<u8>)> {
        self.ui.input_mut().events.extend(events);
        self.ui.run_steps(1);
        self.writes()
    }

    fn writes(&mut self) -> Vec<(SessionId, Vec<u8>)> {
        drain_protocol(self.ui.state_mut())
            .into_iter()
            .filter_map(|command| match command {
                RuntimeCommand::WriteInput { session, bytes } => Some((session, bytes)),
                _ => None,
            })
            .collect()
    }

    fn replace_focused_session(&mut self, session: SessionId) -> Vec<(SessionId, Vec<u8>)> {
        let workspace = self.ui.state_mut();
        workspace.begin_terminal_refocus(pane_id("pane"));
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("pane", session)],
                LayoutNode::Pane(pane_id("pane")),
            )],
            "pane",
        ));
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        self.ui.run_steps(1);
        self.writes()
    }

    fn focus_new_pane_session(&mut self, session: SessionId) -> Vec<(SessionId, Vec<u8>)> {
        let workspace = self.ui.state_mut();
        workspace.begin_terminal_refocus(pane_id("next"));
        workspace.mux = Some(mux(
            "primary",
            vec![tab(
                "primary",
                vec![pane("next", session)],
                LayoutNode::Pane(pane_id("next")),
            )],
            "next",
        ));
        workspace.sessions.entry(session).or_default().snapshot = Some(snapshot("ready"));
        self.ui.run_steps(1);
        self.writes()
    }
}

fn enter() -> egui::Event {
    egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

#[test]
fn ambiguous_suffix_text_without_native_monitor_stays_available_to_new_pane() {
    let bare_suffix = [commit_event("요."), egui::Event::Text(".".into())];
    assert_eq!(
        paired_old_ime_text_echo_index(&bare_suffix, 0, "요.", false),
        None
    );
    assert_eq!(
        paired_old_ime_text_echo_index(&bare_suffix, 0, "요.", true),
        Some(1)
    );
    let full_echo = [commit_event("요."), egui::Event::Text("요.".into())];
    assert_eq!(
        paired_old_ime_text_echo_index(&full_echo, 0, "요.", false),
        Some(1)
    );
    let paired_key = egui::Event::Key {
        key: egui::Key::Period,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let key_and_echo = [
        commit_event("요."),
        paired_key,
        egui::Event::Text(".".into()),
    ];
    assert_eq!(
        paired_old_ime_text_echo_index(&key_and_echo, 0, "요.", false),
        Some(2)
    );
}

#[test]
fn late_ime_commit_after_focus_switch_stays_with_original_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([commit_event("요")]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
}

#[test]
fn late_commit_with_punctuation_is_completed_before_old_session_enter() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn runtime_focus_snapshot_does_not_flush_detached_composition() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.focus_new_pane_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn paired_text_echo_of_late_commit_is_not_sent_to_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([commit_event("요."), egui::Event::Text(".".into())]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn queued_punctuation_overlap_with_late_commit_is_not_duplicated() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    trace
        .ui
        .state_mut()
        .pending_ime_submit
        .as_mut()
        .expect("Enter is deferred")
        .before_submit = b".,".to_vec();
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![(SessionId(7), "요.,\r".as_bytes().to_vec())]
    );
}

#[test]
fn native_punctuation_in_late_commit_is_not_replayed_to_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    let old_enter = trace
        .ui
        .state()
        .detached_ime_submit
        .as_ref()
        .unwrap()
        .started;
    trace.ui.state_mut().test_native_key_downs = vec![
        crate::native_key_monitor::NativePrintableKeyDown::for_test_observed_at(
            '.',
            old_enter - std::time::Duration::from_millis(1),
        ),
    ];
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn new_pane_physical_punctuation_is_not_attributed_to_old_commit() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace.ui.state_mut().test_native_key_downs =
        vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![
            (SessionId(7), "요.\r".as_bytes().to_vec()),
            (SessionId(8), b".".to_vec()),
        ]
    );
}

#[test]
fn independent_new_pane_text_after_old_commit_is_preserved() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace.ui.state_mut().test_native_key_downs =
        vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
    assert_eq!(
        trace.frame([commit_event("요."), egui::Event::Text(".".into())]),
        vec![
            (SessionId(7), "요.\r".as_bytes().to_vec()),
            (SessionId(8), b".".to_vec()),
        ]
    );
}

#[test]
fn text_edit_owned_commit_does_not_resolve_detached_terminal_input() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    let foreign = egui::Id::new("foreign-text-edit");
    egui::text_edit::TextEditState::default().store(&trace.ui.ctx, foreign);
    trace
        .ui
        .ctx
        .memory_mut(|memory| memory.request_focus(foreign));
    trace.ui.state_mut().pending_focus = Some(pane_id("pane"));
    assert!(trace.ui.ctx.text_edit_focused());
    assert!(trace.frame([commit_event("요.")]).is_empty());
    assert!(trace.ui.state().detached_ime_submit.is_some());
}

#[test]
fn detached_submit_expires_even_while_text_edit_owns_keyboard() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    let foreign = egui::Id::new("foreign-text-edit-timeout");
    egui::text_edit::TextEditState::default().store(&trace.ui.ctx, foreign);
    trace
        .ui
        .ctx
        .memory_mut(|memory| memory.request_focus(foreign));
    trace.ui.state_mut().pending_focus = None;
    trace
        .ui
        .state_mut()
        .detached_ime_submit
        .as_mut()
        .unwrap()
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
}

#[test]
fn clipboard_result_after_detach_waits_behind_old_enter() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    trace.ui.state_mut().request_terminal_clipboard(
        SessionId(7),
        false,
        crate::ui::file_tree::ShellKind::Posix,
        None,
    );
    let Some(WorkspaceIoIntent::ReadTerminalClipboard {
        operation,
        generation,
    }) = trace.ui.state_mut().take_io_intent()
    else {
        panic!("clipboard request missing");
    };
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation,
            result: TerminalClipboardPayload::try_new(Vec::new(), Some("paste".into())),
        });
    assert!(trace.writes().is_empty());
    assert_eq!(
        trace.frame([commit_event("요")]),
        vec![(SessionId(7), "요\rpaste".as_bytes().to_vec())]
    );
}

#[test]
fn paired_key_of_late_commit_is_not_replayed_in_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    let period = egui::Event::Key {
        key: egui::Key::Period,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert_eq!(
        trace.frame([period, commit_event("요.")]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn physical_new_pane_key_next_to_old_commit_is_preserved() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace.ui.state_mut().test_native_key_downs =
        vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
    let period = egui::Event::Key {
        key: egui::Key::Period,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert_eq!(
        trace.frame([period, commit_event("요.")]),
        vec![
            (SessionId(7), "요.\r".as_bytes().to_vec()),
            (SessionId(8), b".".to_vec()),
        ]
    );
}

#[test]
fn paired_key_and_text_after_timeout_stay_out_of_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .detached_ime_submit
        .as_mut()
        .unwrap()
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
    let period = egui::Event::Key {
        key: egui::Key::Period,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert!(
        trace
            .frame([period, commit_event("요."), egui::Event::Text(".".into())])
            .is_empty()
    );
}

#[test]
fn full_old_commit_echo_does_not_swallow_independent_new_physical_key() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace.ui.state_mut().test_native_key_downs =
        vec![crate::native_key_monitor::NativePrintableKeyDown::for_test(
            '.',
        )];
    assert_eq!(
        trace.frame([commit_event("요."), egui::Event::Text("요.".into())]),
        vec![
            (SessionId(7), "요.\r".as_bytes().to_vec()),
            (SessionId(8), b".".to_vec()),
        ]
    );
}

#[test]
fn key_between_old_commit_and_text_echo_does_not_leak() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    let period = egui::Event::Key {
        key: egui::Key::Period,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert_eq!(
        trace.frame([commit_event("요."), period, egui::Event::Text(".".into())]),
        vec![(SessionId(7), "요.\r".as_bytes().to_vec())]
    );
}

#[test]
fn clipboard_before_enter_is_not_deduplicated_against_late_commit() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    trace.ui.state_mut().request_terminal_clipboard(
        SessionId(7),
        false,
        crate::ui::file_tree::ShellKind::Posix,
        None,
    );
    let Some(WorkspaceIoIntent::ReadTerminalClipboard {
        operation,
        generation,
    }) = trace.ui.state_mut().take_io_intent()
    else {
        panic!("clipboard request missing");
    };
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .complete_io(WorkspaceIoCompletion::TerminalClipboardRead {
            operation,
            generation,
            result: TerminalClipboardPayload::try_new(Vec::new(), Some(".".into())),
        });
    assert!(trace.writes().is_empty());
    assert_eq!(
        trace.frame([commit_event("요.")]),
        vec![(SessionId(7), "요..\r".as_bytes().to_vec())]
    );
}

#[test]
fn text_echo_after_detached_timeout_does_not_enter_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .detached_ime_submit
        .as_mut()
        .unwrap()
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
    assert!(
        trace
            .frame([commit_event("요."), egui::Event::Text(".".into())])
            .is_empty()
    );
}

#[test]
fn detached_submit_without_commit_expires_into_original_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .detached_ime_submit
        .as_mut()
        .expect("detached Enter is waiting")
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
}

#[test]
fn commit_after_detached_timeout_never_enters_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace
        .ui
        .state_mut()
        .detached_ime_submit
        .as_mut()
        .expect("detached Enter is waiting")
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
    assert!(trace.frame([commit_event("요.")]).is_empty());
}

#[test]
fn leaving_workspace_flushes_detached_submit_without_losing_input() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    trace.ui.state_mut().flush_pending_ime_submit();
    assert_eq!(
        trace.writes(),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
}

#[test]
fn fresh_composition_after_focus_switch_reaches_new_session() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    assert!(trace.replace_focused_session(SessionId(8)).is_empty());
    assert_eq!(
        trace.frame([preedit_event("요"), commit_event("요")]),
        vec![
            (SessionId(7), "요\r".as_bytes().to_vec()),
            (SessionId(8), "요".as_bytes().to_vec()),
        ]
    );
}

#[test]
fn rapid_frames_preserve_commit_enter_and_following_key_order() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("한")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    let tab = egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert!(trace.frame([tab]).is_empty());
    assert_eq!(
        trace.frame([commit_event("한")]),
        vec![(SessionId(7), "한\r\t".as_bytes().to_vec())]
    );
}

#[test]
fn ordinary_navigation_without_composition_reaches_pty() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    let left = egui::Event::Key {
        key: egui::Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    assert_eq!(
        trace.frame([left]),
        vec![(SessionId(7), b"\x1b[D".to_vec())]
    );
}

#[test]
fn missing_commit_timeout_preserves_visible_korean_syllable() {
    let mut trace = TerminalInputHarness::new(SessionId(7));
    assert!(trace.frame([preedit_event("요")]).is_empty());
    assert!(trace.frame([enter(), preedit_event("")]).is_empty());
    trace
        .ui
        .state_mut()
        .pending_ime_submit
        .as_mut()
        .expect("Enter is deferred")
        .started = std::time::Instant::now() - std::time::Duration::from_secs(3);
    assert_eq!(
        trace.frame([]),
        vec![(SessionId(7), "요\r".as_bytes().to_vec())]
    );
}
