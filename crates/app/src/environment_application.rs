//! 프로젝트 환경 전달과 실제 실행 버전을 구분한다. 값이나 자격증명은 저장하지 않는다.
use std::collections::HashMap;

#[derive(Default)]
pub struct EnvironmentApplication {
    settings_generation: u64,
    pub current: Option<u64>,
    pub delivered: Option<u64>,
    pub pending: bool,
    pub failed: bool,
    launch_failed: bool,
    attached: std::collections::HashSet<runtime::SessionId>,
    pub sessions: HashMap<runtime::SessionId, Option<u64>>,
}

#[derive(Default)]
pub struct ApplicationView {
    pub current: Option<u64>,
    pub ready: bool,
    pub pending: bool,
    pub failed: bool,
    pub sessions: Vec<(String, Option<u64>)>,
}

impl EnvironmentApplication {
    pub fn ready(&self) -> bool {
        self.current.is_some()
            && self.current == self.delivered
            && !self.pending
            && !self.failed
            && !self.launch_failed
    }

    pub fn settings_generation(&self) -> u64 {
        self.settings_generation
    }

    pub fn accepts_settings_generation(&self, captured: u64) -> bool {
        captured == self.settings_generation
    }

    pub fn changed(&mut self) {
        self.settings_generation = self.settings_generation.wrapping_add(1);
        self.clear_prepared();
    }

    fn clear_prepared(&mut self) {
        self.current = None;
        self.delivered = None;
        self.failed = false;
        self.launch_failed = false;
        self.pending = false;
    }
    /// 완료한 버전은 유지하고, 취소된 조회만 다음 실행에서 다시 확인한다.
    pub fn cancel_pending(&mut self) -> bool {
        let pending = self.pending;
        if pending {
            self.clear_prepared();
        }
        pending
    }

    pub fn begin(&mut self) {
        self.pending = true;
    }
    pub fn fail(&mut self) {
        self.pending = false;
        self.failed = true;
    }
    pub fn synced(&mut self, revision: u64, failed: bool) {
        self.current = Some(revision);
        self.pending = false;
        self.failed = failed;
    }
    pub fn unchanged(&mut self) {
        self.pending = false;
    }

    pub fn observe(&mut self, events: &[runtime::RuntimeEvent]) {
        for event in events {
            match event {
                runtime::RuntimeEvent::EnvironmentApplied {
                    session: None,
                    revision,
                } => {
                    if *revision == self.current {
                        self.delivered = *revision;
                    }
                }
                runtime::RuntimeEvent::EnvironmentApplied {
                    session: Some(session),
                    revision,
                } => {
                    if *revision == self.current && revision.is_some() {
                        self.launch_failed = false;
                    }
                    if self.sessions.len() < 256 || self.sessions.contains_key(session) {
                        self.sessions.insert(*session, *revision);
                    }
                }
                runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                    let present: std::collections::HashSet<_> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    // spawn ACK는 pane 연결보다 먼저 올 수 있다. 아직 연결 전인 새 세션은 보존한다.
                    self.sessions
                        .retain(|id, _| !self.attached.contains(id) || present.contains(id));
                    self.attached = present;
                    for id in self.attached.iter().take(256) {
                        self.sessions.entry(*id).or_insert(None);
                    }
                }
                runtime::RuntimeEvent::SessionExited { session, .. }
                    if !self.attached.contains(session) =>
                {
                    self.sessions.remove(session);
                }
                runtime::RuntimeEvent::SpawnFailed { message, .. }
                    if matches!(
                        message.message_id.as_str(),
                        "runtime.spawn_failed.shell_secret" | "runtime.spawn_failed.agent_secret"
                    ) =>
                {
                    self.launch_failed = true
                }
                _ => {}
            }
        }
    }

    pub fn view(&self, mut label: impl FnMut(runtime::SessionId) -> String) -> ApplicationView {
        let mut entries: Vec<_> = self
            .sessions
            .iter()
            .filter(|(id, _)| self.attached.contains(id))
            .collect();
        entries.sort_by_key(|(id, _)| id.0);
        ApplicationView {
            current: self.current,
            ready: self.ready(),
            pending: self.pending,
            failed: self.failed || self.launch_failed,
            sessions: entries
                .into_iter()
                .map(|(id, revision)| (label(*id), *revision))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_review_설정변경은_늦은_실행준비_결과를_거부한다() {
        let mut workspace = EnvironmentApplication::default();
        let before = workspace.settings_generation();
        assert!(workspace.accepts_settings_generation(before));
        workspace.changed();
        assert!(!workspace.accepts_settings_generation(before));
        let after = workspace.settings_generation();
        workspace.begin();
        workspace.cancel_pending();
        assert!(
            workspace.accepts_settings_generation(after),
            "조회 취소는 설정 변경이 아니다"
        );
        workspace.changed();
        assert!(!workspace.accepts_settings_generation(after));
    }

    #[test]
    fn environment_application_완료후_전환은_확정버전을_보존한다() {
        let mut state = EnvironmentApplication::default();
        state.synced(7, false);
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: None,
            revision: Some(7),
        }]);
        assert!(!state.cancel_pending());
        assert!(state.ready());
        state.begin();
        assert!(state.cancel_pending());
        assert!(!state.pending);
        assert!(!state.ready());
    }

    #[test]
    fn environment_application_재실행_성공은_실행실패만_해제한다() {
        let mut state = EnvironmentApplication::default();
        state.synced(3, false);
        state.observe(&[
            runtime::RuntimeEvent::EnvironmentApplied {
                session: None,
                revision: Some(3),
            },
            runtime::RuntimeEvent::SpawnFailed {
                kind: runtime::SpawnKind::Agent,
                message: runtime::MessagePayload::new("runtime.spawn_failed.agent_secret"),
            },
        ]);
        assert!(!state.ready());
        state.unchanged();
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: Some(runtime::SessionId(1)),
            revision: Some(3),
        }]);
        assert!(state.ready());
        state.fail();
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: Some(runtime::SessionId(2)),
            revision: Some(3),
        }]);
        assert!(!state.ready());
    }

    #[test]
    fn environment_application_spawn과_pane연결_사이_ack를_보존한다() {
        let empty = || runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: Vec::new(),
                active_tab: None,
                focused_pane: None,
            }),
        };
        let mut state = EnvironmentApplication::default();
        let id = runtime::SessionId(1);
        state.observe(&[
            runtime::RuntimeEvent::EnvironmentApplied {
                session: Some(id),
                revision: Some(42),
            },
            empty(),
        ]);
        assert_eq!(state.sessions.get(&id), Some(&Some(42)));
        let pane = deppy_core::MuxPaneId("p".into());
        state.observe(&[runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: deppy_core::MuxTabId("t".into()),
                    title: "tab".into(),
                    layout: runtime::LayoutNode::Pane(pane.clone()),
                    panes: vec![runtime::PaneSnapshot {
                        id: pane,
                        title: "pane".into(),
                        session_id: Some(id),
                        persistent_session_id: None,
                    }],
                }],
                active_tab: None,
                focused_pane: None,
            }),
        }]);
        assert_eq!(state.view(|_| "session".into()).sessions[0].1, Some(42));
        state.observe(&[empty()]);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn environment_application_미조회는_준비됨이_아니다() {
        assert!(!EnvironmentApplication::default().ready());
    }
    #[test]
    fn environment_application_기본값_ack는_기존_실행버전을_바꾸지_않는다() {
        let mut state = EnvironmentApplication::default();
        state.synced(1, false);
        assert!(!state.ready());
        state.observe(&[
            runtime::RuntimeEvent::EnvironmentApplied {
                session: None,
                revision: Some(1),
            },
            runtime::RuntimeEvent::EnvironmentApplied {
                session: Some(runtime::SessionId(1)),
                revision: Some(1),
            },
        ]);
        assert!(state.ready());
        state.begin();
        assert!(!state.ready());
        state.synced(2, false);
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: None,
            revision: Some(1),
        }]);
        assert!(!state.ready());
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: None,
            revision: Some(2),
        }]);
        assert!(state.ready());
        assert_eq!(state.sessions[&runtime::SessionId(1)], Some(1));
        state.fail();
        assert!(!state.ready());
        state.synced(2, false);
        assert!(state.ready());
        state.synced(3, true);
        state.observe(&[runtime::RuntimeEvent::EnvironmentApplied {
            session: None,
            revision: Some(3),
        }]);
        assert!(!state.ready());
    }
}
