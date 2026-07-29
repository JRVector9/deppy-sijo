use runtime::{MuxPaneId, MuxTabId, SessionId};

const MIN_ATTACHED_RATIO: f32 = 0.10;
const MAX_ATTACHED_RATIO: f32 = 0.90;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspacePaneTarget {
    pub(crate) workspace_id: String,
    pub(crate) runtime_instance: u64,
    pub(crate) tab: MuxTabId,
    pub(crate) pane: MuxPaneId,
    pub(crate) session: SessionId,
}

impl WorkspacePaneTarget {
    pub(crate) fn new(
        workspace_id: impl Into<String>,
        runtime_instance: u64,
        tab: MuxTabId,
        pane: MuxPaneId,
        session: SessionId,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            runtime_instance,
            tab,
            pane,
            session,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalSurfaceFocus {
    #[default]
    Primary,
    Attached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedPlaceholder {
    Suspended,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedRenderState {
    Live,
    Placeholder(AttachedPlaceholder),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedRuntimeState {
    Live {
        runtime_instance: u64,
        pane_present: bool,
        session_present: bool,
    },
    Suspended {
        runtime_instance: u64,
    },
    Disconnected {
        runtime_instance: Option<u64>,
    },
}

impl AttachedRuntimeState {
    fn runtime_instance(self) -> Option<u64> {
        match self {
            Self::Live {
                runtime_instance, ..
            }
            | Self::Suspended { runtime_instance } => Some(runtime_instance),
            Self::Disconnected { runtime_instance } => runtime_instance,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetachReason {
    PrimaryWorkspaceChanged,
    RuntimeReplaced,
    PaneMissing,
    SessionMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileDecision {
    NoAttachment,
    RetainedLive,
    Placeholder(AttachedPlaceholder),
    Detached(DetachReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DetachDecision {
    target: WorkspacePaneTarget,
}

impl DetachDecision {
    pub(crate) fn target(&self) -> &WorkspacePaneTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachedPane {
    primary_workspace_id: String,
    target: WorkspacePaneTarget,
    ratio: f32,
    render_state: AttachedRenderState,
}

impl AttachedPane {
    pub(crate) fn target(&self) -> &WorkspacePaneTarget {
        &self.target
    }

    pub(crate) fn ratio(&self) -> f32 {
        self.ratio
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusedInputTarget<'a> {
    Primary,
    Attached(&'a WorkspacePaneTarget),
}

#[derive(Debug, Default)]
pub(crate) struct CrossWorkspacePaneState {
    attachment: Option<AttachedPane>,
    focused_surface: TerminalSurfaceFocus,
}

impl CrossWorkspacePaneState {
    pub(crate) fn attach(
        &mut self,
        primary_workspace_id: impl Into<String>,
        target: WorkspacePaneTarget,
        ratio: f32,
    ) {
        self.attachment = Some(AttachedPane {
            primary_workspace_id: primary_workspace_id.into(),
            target,
            ratio: ratio.clamp(MIN_ATTACHED_RATIO, MAX_ATTACHED_RATIO),
            render_state: AttachedRenderState::Live,
        });
        self.focused_surface = TerminalSurfaceFocus::Attached;
    }

    pub(crate) fn detach(&mut self) -> Option<DetachDecision> {
        let attachment = self.attachment.take()?;
        self.focused_surface = TerminalSurfaceFocus::Primary;
        Some(DetachDecision {
            target: attachment.target,
        })
    }

    pub(crate) fn reconcile(
        &mut self,
        current_primary_workspace_id: &str,
        runtime: AttachedRuntimeState,
    ) -> ReconcileDecision {
        let Some(attachment) = self.attachment.as_ref() else {
            return ReconcileDecision::NoAttachment;
        };
        if attachment.primary_workspace_id != current_primary_workspace_id {
            return self.detach_for(DetachReason::PrimaryWorkspaceChanged);
        }
        if runtime
            .runtime_instance()
            .is_some_and(|instance| instance != attachment.target.runtime_instance)
        {
            return self.detach_for(DetachReason::RuntimeReplaced);
        }

        match runtime {
            AttachedRuntimeState::Live {
                pane_present: false,
                ..
            } => self.detach_for(DetachReason::PaneMissing),
            AttachedRuntimeState::Live {
                session_present: false,
                ..
            } => self.detach_for(DetachReason::SessionMissing),
            AttachedRuntimeState::Live { .. } => {
                self.attachment.as_mut().unwrap().render_state = AttachedRenderState::Live;
                ReconcileDecision::RetainedLive
            }
            AttachedRuntimeState::Suspended { .. } => {
                self.retain_placeholder(AttachedPlaceholder::Suspended)
            }
            AttachedRuntimeState::Disconnected { .. } => {
                self.retain_placeholder(AttachedPlaceholder::Disconnected)
            }
        }
    }

    pub(crate) fn attachment(&self) -> Option<&AttachedPane> {
        self.attachment.as_ref()
    }

    pub(crate) fn set_ratio(&mut self, ratio: f32) {
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.ratio = ratio.clamp(MIN_ATTACHED_RATIO, MAX_ATTACHED_RATIO);
        }
    }

    pub(crate) fn focused_surface(&self) -> TerminalSurfaceFocus {
        self.focused_surface
    }

    pub(crate) fn focus_primary(&mut self) {
        self.focused_surface = TerminalSurfaceFocus::Primary;
    }

    pub(crate) fn focus_attached(&mut self) {
        if self.attachment.is_some() {
            self.focused_surface = TerminalSurfaceFocus::Attached;
        }
    }

    pub(crate) fn render_state(&self) -> Option<AttachedRenderState> {
        self.attachment
            .as_ref()
            .map(|attachment| attachment.render_state)
    }

    pub(crate) fn primary_input_enabled(&self) -> bool {
        self.focused_surface == TerminalSurfaceFocus::Primary
    }

    pub(crate) fn attached_input_enabled(&self) -> bool {
        self.focused_surface == TerminalSurfaceFocus::Attached
            && self.render_state() == Some(AttachedRenderState::Live)
    }

    pub(crate) fn focused_input_target(&self) -> Option<FocusedInputTarget<'_>> {
        match self.focused_surface {
            TerminalSurfaceFocus::Primary => Some(FocusedInputTarget::Primary),
            TerminalSurfaceFocus::Attached if self.attached_input_enabled() => self
                .attachment
                .as_ref()
                .map(|attachment| FocusedInputTarget::Attached(&attachment.target)),
            TerminalSurfaceFocus::Attached => None,
        }
    }

    pub(crate) fn protects_runtime(&self, workspace_id: &str, runtime_instance: u64) -> bool {
        self.attachment.as_ref().is_some_and(|attachment| {
            attachment.target.workspace_id == workspace_id
                && attachment.target.runtime_instance == runtime_instance
        })
    }

    fn detach_for(&mut self, reason: DetachReason) -> ReconcileDecision {
        let _ = self.detach();
        ReconcileDecision::Detached(reason)
    }

    fn retain_placeholder(&mut self, placeholder: AttachedPlaceholder) -> ReconcileDecision {
        self.attachment.as_mut().unwrap().render_state =
            AttachedRenderState::Placeholder(placeholder);
        ReconcileDecision::Placeholder(placeholder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{MuxPaneId, MuxTabId, SessionId};

    fn target(workspace_id: &str, runtime_instance: u64) -> WorkspacePaneTarget {
        WorkspacePaneTarget::new(
            workspace_id,
            runtime_instance,
            MuxTabId("shared-tab".to_owned()),
            MuxPaneId("shared-pane".to_owned()),
            SessionId(7),
        )
    }

    fn live(runtime_instance: u64) -> AttachedRuntimeState {
        AttachedRuntimeState::Live {
            runtime_instance,
            pane_present: true,
            session_present: true,
        }
    }

    #[test]
    fn target_namespace_includes_workspace_and_runtime_instance() {
        let a = target("workspace-a", 1);
        let b = target("workspace-b", 1);
        let replacement = target("workspace-a", 2);

        assert_ne!(a, b);
        assert_ne!(a, replacement);
    }

    #[test]
    fn attach_replaces_previous_target_and_focuses_attached() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.40);
        state.attach("workspace-a", target("workspace-c", 2), 0.60);

        assert_eq!(
            state.attachment().unwrap().target(),
            &target("workspace-c", 2)
        );
        assert_eq!(state.focused_surface(), TerminalSurfaceFocus::Attached);
    }

    #[test]
    fn ratio_clamps_at_both_boundaries() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), -1.0);
        assert_eq!(state.attachment().unwrap().ratio(), 0.10);

        state.set_ratio(2.0);
        assert_eq!(state.attachment().unwrap().ratio(), 0.90);

        state.set_ratio(0.37);
        assert_eq!(state.attachment().unwrap().ratio(), 0.37);
    }

    #[test]
    fn detach_is_non_destructive_and_restores_primary_focus() {
        let mut state = CrossWorkspacePaneState::default();
        let attached = target("workspace-b", 1);
        state.attach("workspace-a", attached.clone(), 0.5);

        let decision = state.detach().unwrap();

        assert_eq!(decision.target(), &attached);
        assert!(state.attachment().is_none());
        assert_eq!(state.focused_surface(), TerminalSurfaceFocus::Primary);
        let DetachDecision { target } = decision;
        assert_eq!(target, attached);
    }

    #[test]
    fn workspace_switch_auto_detaches() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.5);

        assert_eq!(
            state.reconcile("workspace-c", live(1)),
            ReconcileDecision::Detached(DetachReason::PrimaryWorkspaceChanged)
        );
        assert!(state.attachment().is_none());
    }

    #[test]
    fn runtime_replacement_auto_detaches_even_when_ids_match() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.5);

        assert_eq!(
            state.reconcile("workspace-a", live(2)),
            ReconcileDecision::Detached(DetachReason::RuntimeReplaced)
        );
    }

    #[test]
    fn missing_pane_or_session_auto_detaches() {
        for (pane_present, session_present, expected) in [
            (false, true, DetachReason::PaneMissing),
            (true, false, DetachReason::SessionMissing),
        ] {
            let mut state = CrossWorkspacePaneState::default();
            state.attach("workspace-a", target("workspace-b", 1), 0.5);

            assert_eq!(
                state.reconcile(
                    "workspace-a",
                    AttachedRuntimeState::Live {
                        runtime_instance: 1,
                        pane_present,
                        session_present,
                    },
                ),
                ReconcileDecision::Detached(expected)
            );
        }
    }

    #[test]
    fn suspended_and_disconnected_keep_placeholder_without_input() {
        for (runtime, placeholder) in [
            (
                AttachedRuntimeState::Suspended {
                    runtime_instance: 1,
                },
                AttachedPlaceholder::Suspended,
            ),
            (
                AttachedRuntimeState::Disconnected {
                    runtime_instance: Some(1),
                },
                AttachedPlaceholder::Disconnected,
            ),
        ] {
            let mut state = CrossWorkspacePaneState::default();
            state.attach("workspace-a", target("workspace-b", 1), 0.5);

            assert_eq!(
                state.reconcile("workspace-a", runtime),
                ReconcileDecision::Placeholder(placeholder)
            );
            assert_eq!(
                state.render_state(),
                Some(AttachedRenderState::Placeholder(placeholder))
            );
            assert!(!state.primary_input_enabled());
            assert!(!state.attached_input_enabled());
            assert_eq!(state.focused_input_target(), None);
        }
    }

    #[test]
    fn valid_live_target_is_retained_and_routable() {
        let mut state = CrossWorkspacePaneState::default();
        let attached = target("workspace-b", 1);
        state.attach("workspace-a", attached.clone(), 0.5);

        assert_eq!(
            state.reconcile("workspace-a", live(1)),
            ReconcileDecision::RetainedLive
        );
        assert_eq!(state.render_state(), Some(AttachedRenderState::Live));
        assert!(!state.primary_input_enabled());
        assert!(state.attached_input_enabled());
        assert_eq!(
            state.focused_input_target(),
            Some(FocusedInputTarget::Attached(&attached))
        );
    }

    #[test]
    fn live_attachment_enables_exactly_one_focused_surface() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.5);

        assert!(!state.primary_input_enabled());
        assert!(state.attached_input_enabled());

        state.focus_primary();

        assert!(state.primary_input_enabled());
        assert!(!state.attached_input_enabled());
    }

    #[test]
    fn primary_focus_routes_to_primary_but_unavailable_attached_does_not_fallback() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.5);
        state.focus_primary();
        assert!(state.primary_input_enabled());
        assert!(!state.attached_input_enabled());
        assert_eq!(
            state.focused_input_target(),
            Some(FocusedInputTarget::Primary)
        );

        state.focus_attached();
        state.reconcile(
            "workspace-a",
            AttachedRuntimeState::Disconnected {
                runtime_instance: None,
            },
        );
        assert!(!state.primary_input_enabled());
        assert!(!state.attached_input_enabled());
        assert_eq!(state.focused_input_target(), None);
    }

    #[test]
    fn attached_runtime_is_protected_from_warm_eviction() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 9), 0.5);

        assert!(state.protects_runtime("workspace-b", 9));
        assert!(!state.protects_runtime("workspace-a", 9));
        assert!(!state.protects_runtime("workspace-b", 10));
    }
}
