use runtime::{MuxPaneId, MuxTabId, SessionId};

const MIN_ATTACHED_RATIO: f32 = 0.10;
const MAX_ATTACHED_RATIO: f32 = 0.90;
const DEFAULT_ATTACHED_RATIO: f32 = 0.50;
const MIN_ATTACHED_WIDTH_PX: f32 = 320.0;
const MAX_ATTACHED_WIDTH_PX: f32 = 960.0;
const DEFAULT_ATTACHED_WIDTH_PX: f32 = 420.0;

pub(crate) const HARD_MAX_CROSS_WORKSPACE_PANES: usize = 6;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PersistedPaneRequest {
    workspace_id: String,
    pane: MuxPaneId,
}

impl PersistedPaneRequest {
    pub(crate) fn new(workspace_id: impl Into<String>, pane: MuxPaneId) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            pane,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalSurfaceFocus {
    #[default]
    Primary,
    Attached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AttachmentId(u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FocusedSurface {
    #[default]
    Primary,
    Attached(AttachmentId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachOutcome {
    Appended(AttachmentId),
    FocusedExisting(AttachmentId),
    CapacityReached,
}

#[cfg(test)]
impl AttachOutcome {
    pub(crate) fn appended_id(self) -> Option<AttachmentId> {
        match self {
            Self::Appended(id) => Some(id),
            Self::FocusedExisting(_) | Self::CapacityReached => None,
        }
    }
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
pub(crate) enum LiveTargetRelation {
    Exact,
    PaneMissing,
    SessionMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachedRuntimeState {
    Live {
        runtime_instance: u64,
        target_relation: LiveTargetRelation,
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
    source: AttachedPaneSource,
}

impl DetachDecision {
    pub(crate) fn target(&self) -> &WorkspacePaneTarget {
        self.live_target()
            .expect("restoring attachments do not have a live target")
    }

    pub(crate) fn live_target(&self) -> Option<&WorkspacePaneTarget> {
        match &self.source {
            AttachedPaneSource::Live(target) => Some(target),
            AttachedPaneSource::Restoring(_) => None,
        }
    }

    pub(crate) fn restoring_request(&self) -> Option<&PersistedPaneRequest> {
        match &self.source {
            AttachedPaneSource::Restoring(request) => Some(request),
            AttachedPaneSource::Live(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AttachedPaneSource {
    Restoring(PersistedPaneRequest),
    Live(WorkspacePaneTarget),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachedPane {
    id: AttachmentId,
    primary_workspace_id: String,
    source: AttachedPaneSource,
    width_px: f32,
    ratio: f32,
    render_state: AttachedRenderState,
}

impl AttachedPane {
    pub(crate) fn id(&self) -> AttachmentId {
        self.id
    }

    pub(crate) fn target(&self) -> &WorkspacePaneTarget {
        self.live_target()
            .expect("restoring attachments do not have a live target")
    }

    pub(crate) fn live_target(&self) -> Option<&WorkspacePaneTarget> {
        match &self.source {
            AttachedPaneSource::Live(target) => Some(target),
            AttachedPaneSource::Restoring(_) => None,
        }
    }

    pub(crate) fn restoring_request(&self) -> Option<&PersistedPaneRequest> {
        match &self.source {
            AttachedPaneSource::Restoring(request) => Some(request),
            AttachedPaneSource::Live(_) => None,
        }
    }

    pub(crate) fn width_px(&self) -> f32 {
        self.width_px
    }

    fn matches_canonical_pane(&self, workspace_id: &str, pane: &MuxPaneId) -> bool {
        match &self.source {
            AttachedPaneSource::Restoring(request) => {
                request.workspace_id == workspace_id && request.pane == *pane
            }
            AttachedPaneSource::Live(target) => {
                target.workspace_id == workspace_id && target.pane == *pane
            }
        }
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

#[derive(Debug)]
pub(crate) struct CrossWorkspacePaneState {
    attachments: Vec<AttachedPane>,
    focused: FocusedSurface,
    next_id: u64,
}

impl Default for CrossWorkspacePaneState {
    fn default() -> Self {
        Self {
            attachments: Vec::new(),
            focused: FocusedSurface::Primary,
            next_id: 1,
        }
    }
}

impl CrossWorkspacePaneState {
    pub(crate) fn attach(
        &mut self,
        primary_workspace_id: impl Into<String>,
        target: WorkspacePaneTarget,
        ratio: f32,
    ) {
        self.attachments.clear();
        self.focused = FocusedSurface::Primary;
        let normalized_ratio = normalize_ratio(ratio);
        let primary_workspace_id = primary_workspace_id.into();
        let id = self.allocate_id();
        self.attachments.push(AttachedPane {
            id,
            primary_workspace_id,
            source: AttachedPaneSource::Live(target),
            width_px: DEFAULT_ATTACHED_WIDTH_PX,
            ratio: normalized_ratio,
            render_state: AttachedRenderState::Live,
        });
        self.focused = FocusedSurface::Attached(id);
    }

    pub(crate) fn attach_right(
        &mut self,
        primary_workspace_id: impl Into<String>,
        target: WorkspacePaneTarget,
        width_px: f32,
        limit: usize,
    ) -> AttachOutcome {
        if let Some(existing) = self.attachments.iter().find(|attachment| {
            attachment.matches_canonical_pane(&target.workspace_id, &target.pane)
        }) {
            self.focused = FocusedSurface::Attached(existing.id);
            return AttachOutcome::FocusedExisting(existing.id);
        }
        if self.attachments.len() >= limit.min(HARD_MAX_CROSS_WORKSPACE_PANES) {
            return AttachOutcome::CapacityReached;
        }

        let id = self.allocate_id();
        self.attachments.push(AttachedPane {
            id,
            primary_workspace_id: primary_workspace_id.into(),
            source: AttachedPaneSource::Live(target),
            width_px: normalize_width(width_px),
            ratio: DEFAULT_ATTACHED_RATIO,
            render_state: AttachedRenderState::Live,
        });
        self.focused = FocusedSurface::Attached(id);
        AttachOutcome::Appended(id)
    }

    pub(crate) fn append_restoring(
        &mut self,
        primary_workspace_id: impl Into<String>,
        request: PersistedPaneRequest,
        width_px: f32,
        limit: usize,
    ) -> AttachOutcome {
        if let Some(existing) = self.attachments.iter().find(|attachment| {
            attachment.matches_canonical_pane(&request.workspace_id, &request.pane)
        }) {
            self.focused = FocusedSurface::Attached(existing.id);
            return AttachOutcome::FocusedExisting(existing.id);
        }
        if self.attachments.len() >= limit.min(HARD_MAX_CROSS_WORKSPACE_PANES) {
            return AttachOutcome::CapacityReached;
        }

        let id = self.allocate_id();
        self.attachments.push(AttachedPane {
            id,
            primary_workspace_id: primary_workspace_id.into(),
            source: AttachedPaneSource::Restoring(request),
            width_px: normalize_width(width_px),
            ratio: DEFAULT_ATTACHED_RATIO,
            render_state: AttachedRenderState::Placeholder(AttachedPlaceholder::Disconnected),
        });
        self.focused = FocusedSurface::Attached(id);
        AttachOutcome::Appended(id)
    }

    pub(crate) fn promote_restoring(
        &mut self,
        id: AttachmentId,
        target: WorkspacePaneTarget,
    ) -> bool {
        let Some(attachment) = self.attachment_mut(id) else {
            return false;
        };
        let AttachedPaneSource::Restoring(request) = &attachment.source else {
            return false;
        };
        if request.workspace_id != target.workspace_id || request.pane != target.pane {
            return false;
        }
        attachment.source = AttachedPaneSource::Live(target);
        attachment.render_state = AttachedRenderState::Live;
        true
    }

    pub(crate) fn detach(&mut self) -> Option<DetachDecision> {
        let id = self.attachments.last()?.id;
        self.detach_attachment(id)
    }

    pub(crate) fn detach_attachment(&mut self, id: AttachmentId) -> Option<DetachDecision> {
        let index = self
            .attachments
            .iter()
            .position(|attachment| attachment.id == id)?;
        let attachment = self.attachments.remove(index);
        if self.focused == FocusedSurface::Attached(id) {
            self.focused = FocusedSurface::Primary;
        }
        Some(DetachDecision {
            source: attachment.source,
        })
    }

    pub(crate) fn enforce_capacity(&mut self, limit: usize) -> Vec<DetachDecision> {
        let limit = limit.min(HARD_MAX_CROSS_WORKSPACE_PANES);
        let mut detached = Vec::with_capacity(self.attachments.len().saturating_sub(limit));
        while self.attachments.len() > limit {
            let id = self.attachments.last().unwrap().id;
            detached.push(self.detach_attachment(id).unwrap());
        }
        detached
    }

    pub(crate) fn reorder(&mut self, id: AttachmentId, destination: usize) -> bool {
        let Some(source) = self
            .attachments
            .iter()
            .position(|attachment| attachment.id == id)
        else {
            return false;
        };
        let destination = destination.min(self.attachments.len().saturating_sub(1));
        if source != destination {
            let attachment = self.attachments.remove(source);
            self.attachments.insert(destination, attachment);
        }
        true
    }

    pub(crate) fn reconcile(
        &mut self,
        current_primary_workspace_id: &str,
        runtime: AttachedRuntimeState,
    ) -> ReconcileDecision {
        let Some(id) = self.attachments.first().map(|attachment| attachment.id) else {
            return ReconcileDecision::NoAttachment;
        };
        self.reconcile_target(current_primary_workspace_id, id, runtime)
    }

    pub(crate) fn reconcile_target(
        &mut self,
        current_primary_workspace_id: &str,
        id: AttachmentId,
        runtime: AttachedRuntimeState,
    ) -> ReconcileDecision {
        let Some(attachment) = self
            .attachments
            .iter()
            .find(|attachment| attachment.id == id)
        else {
            return ReconcileDecision::NoAttachment;
        };
        if attachment.primary_workspace_id != current_primary_workspace_id {
            return self.detach_for(id, DetachReason::PrimaryWorkspaceChanged);
        }
        if runtime.runtime_instance().is_some_and(|instance| {
            attachment
                .live_target()
                .is_some_and(|target| instance != target.runtime_instance)
        }) {
            return self.detach_for(id, DetachReason::RuntimeReplaced);
        }
        if attachment.live_target().is_none() {
            return ReconcileDecision::NoAttachment;
        }

        match runtime {
            AttachedRuntimeState::Live {
                target_relation: LiveTargetRelation::PaneMissing,
                ..
            } => self.detach_for(id, DetachReason::PaneMissing),
            AttachedRuntimeState::Live {
                target_relation: LiveTargetRelation::SessionMismatch,
                ..
            } => self.detach_for(id, DetachReason::SessionMissing),
            AttachedRuntimeState::Live {
                target_relation: LiveTargetRelation::Exact,
                ..
            } => {
                self.attachment_mut(id).unwrap().render_state = AttachedRenderState::Live;
                ReconcileDecision::RetainedLive
            }
            AttachedRuntimeState::Suspended { .. } => {
                self.retain_placeholder(id, AttachedPlaceholder::Suspended)
            }
            AttachedRuntimeState::Disconnected { .. } => {
                self.retain_placeholder(id, AttachedPlaceholder::Disconnected)
            }
        }
    }

    pub(crate) fn attachment(&self) -> Option<&AttachedPane> {
        self.attachments.first()
    }

    pub(crate) fn attachments(&self) -> &[AttachedPane] {
        &self.attachments
    }

    pub(crate) fn set_ratio(&mut self, ratio: f32) {
        if ratio.is_finite()
            && let Some(attachment) = self.attachments.first_mut()
        {
            attachment.ratio = ratio.clamp(MIN_ATTACHED_RATIO, MAX_ATTACHED_RATIO);
        }
    }

    pub(crate) fn set_width(&mut self, id: AttachmentId, width_px: f32) -> bool {
        if !width_px.is_finite() {
            return false;
        }
        let Some(attachment) = self.attachment_mut(id) else {
            return false;
        };
        attachment.width_px = width_px.clamp(MIN_ATTACHED_WIDTH_PX, MAX_ATTACHED_WIDTH_PX);
        true
    }

    pub(crate) fn focused_surface(&self) -> TerminalSurfaceFocus {
        match self.focused {
            FocusedSurface::Primary => TerminalSurfaceFocus::Primary,
            FocusedSurface::Attached(_) => TerminalSurfaceFocus::Attached,
        }
    }

    pub(crate) fn focused(&self) -> FocusedSurface {
        self.focused
    }

    pub(crate) fn focus_primary(&mut self) {
        self.focused = FocusedSurface::Primary;
    }

    pub(crate) fn focus_attached(&mut self) {
        if let Some(attachment) = self.attachments.first() {
            self.focused = FocusedSurface::Attached(attachment.id);
        }
    }

    pub(crate) fn focus_attachment(&mut self, id: AttachmentId) -> bool {
        if self
            .attachments
            .iter()
            .any(|attachment| attachment.id == id)
        {
            self.focused = FocusedSurface::Attached(id);
            true
        } else {
            false
        }
    }

    pub(crate) fn render_state(&self) -> Option<AttachedRenderState> {
        self.attachments
            .first()
            .map(|attachment| attachment.render_state)
    }

    pub(crate) fn render_state_for(&self, id: AttachmentId) -> Option<AttachedRenderState> {
        self.attachments
            .iter()
            .find(|attachment| attachment.id == id)
            .map(|attachment| attachment.render_state)
    }

    pub(crate) fn primary_input_enabled(&self) -> bool {
        self.focused == FocusedSurface::Primary
    }

    pub(crate) fn attached_input_enabled(&self) -> bool {
        matches!(
            self.focused,
            FocusedSurface::Attached(id)
                if self.render_state_for(id) == Some(AttachedRenderState::Live)
        )
    }

    pub(crate) fn focused_input_target(&self) -> Option<FocusedInputTarget<'_>> {
        match self.focused {
            FocusedSurface::Primary => Some(FocusedInputTarget::Primary),
            FocusedSurface::Attached(id) if self.attached_input_enabled() => self
                .attachments
                .iter()
                .find(|attachment| attachment.id == id)
                .and_then(AttachedPane::live_target)
                .map(FocusedInputTarget::Attached),
            FocusedSurface::Attached(_) => None,
        }
    }

    pub(crate) fn protects_runtime(&self, workspace_id: &str, runtime_instance: u64) -> bool {
        self.attachments.iter().any(|attachment| {
            attachment.live_target().is_some_and(|target| {
                target.workspace_id == workspace_id && target.runtime_instance == runtime_instance
            })
        })
    }

    fn allocate_id(&mut self) -> AttachmentId {
        let id = AttachmentId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn attachment_mut(&mut self, id: AttachmentId) -> Option<&mut AttachedPane> {
        self.attachments
            .iter_mut()
            .find(|attachment| attachment.id == id)
    }

    fn detach_for(&mut self, id: AttachmentId, reason: DetachReason) -> ReconcileDecision {
        let _ = self.detach_attachment(id);
        ReconcileDecision::Detached(reason)
    }

    fn retain_placeholder(
        &mut self,
        id: AttachmentId,
        placeholder: AttachedPlaceholder,
    ) -> ReconcileDecision {
        self.attachment_mut(id).unwrap().render_state =
            AttachedRenderState::Placeholder(placeholder);
        ReconcileDecision::Placeholder(placeholder)
    }
}

fn normalize_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_ATTACHED_RATIO, MAX_ATTACHED_RATIO)
    } else {
        DEFAULT_ATTACHED_RATIO
    }
}

fn normalize_width(width_px: f32) -> f32 {
    if width_px.is_finite() {
        width_px.clamp(MIN_ATTACHED_WIDTH_PX, MAX_ATTACHED_WIDTH_PX)
    } else {
        DEFAULT_ATTACHED_WIDTH_PX
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
            target_relation: LiveTargetRelation::Exact,
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
    fn non_finite_ratio_uses_safe_attach_default_and_preserves_last_valid_update() {
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut state = CrossWorkspacePaneState::default();
            state.attach("workspace-a", target("workspace-b", 1), invalid);
            assert_eq!(state.attachment().unwrap().ratio(), 0.5);

            state.set_ratio(0.37);
            state.set_ratio(invalid);
            assert_eq!(state.attachment().unwrap().ratio(), 0.37);
        }
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
        assert_eq!(decision.live_target(), Some(&attached));
        assert_eq!(decision.restoring_request(), None);
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
        for (target_relation, expected) in [
            (LiveTargetRelation::PaneMissing, DetachReason::PaneMissing),
            (
                LiveTargetRelation::SessionMismatch,
                DetachReason::SessionMissing,
            ),
        ] {
            let mut state = CrossWorkspacePaneState::default();
            state.attach("workspace-a", target("workspace-b", 1), 0.5);

            assert_eq!(
                state.reconcile(
                    "workspace-a",
                    AttachedRuntimeState::Live {
                        runtime_instance: 1,
                        target_relation,
                    },
                ),
                ReconcileDecision::Detached(expected)
            );
        }
    }

    #[test]
    fn pane_with_different_session_detaches_and_is_never_routable() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach("workspace-a", target("workspace-b", 1), 0.5);

        assert_eq!(
            state.reconcile(
                "workspace-a",
                AttachedRuntimeState::Live {
                    runtime_instance: 1,
                    target_relation: LiveTargetRelation::SessionMismatch,
                },
            ),
            ReconcileDecision::Detached(DetachReason::SessionMissing)
        );
        assert!(state.attachment().is_none());
        assert!(!state.protects_runtime("workspace-b", 1));
        assert_eq!(
            state.focused_input_target(),
            Some(FocusedInputTarget::Primary)
        );
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

    fn distinct_target(
        workspace_id: &str,
        runtime_instance: u64,
        suffix: &str,
    ) -> WorkspacePaneTarget {
        WorkspacePaneTarget::new(
            workspace_id,
            runtime_instance,
            MuxTabId(format!("tab-{suffix}")),
            MuxPaneId(format!("pane-{suffix}")),
            SessionId(runtime_instance + suffix.len() as u64),
        )
    }

    #[test]
    fn attach_right_appends_and_duplicate_focuses_existing() {
        let mut state = CrossWorkspacePaneState::default();
        let first = distinct_target("workspace-b", 1, "first");
        let second = distinct_target("workspace-c", 2, "second");

        let AttachOutcome::Appended(first_id) =
            state.attach_right("workspace-a", first.clone(), 420.0, 6)
        else {
            panic!("first target should append");
        };
        let AttachOutcome::Appended(second_id) =
            state.attach_right("workspace-a", second.clone(), 420.0, 6)
        else {
            panic!("second target should append");
        };

        assert_eq!(
            state
                .attachments()
                .iter()
                .map(AttachedPane::target)
                .collect::<Vec<_>>(),
            vec![&first, &second]
        );
        assert_eq!(state.focused(), FocusedSurface::Attached(second_id));
        assert_eq!(
            state.attach_right("workspace-a", first, 700.0, 6),
            AttachOutcome::FocusedExisting(first_id)
        );
        assert_eq!(state.attachments().len(), 2);
        assert_eq!(state.focused(), FocusedSurface::Attached(first_id));
    }

    #[test]
    fn attachment_focus_survives_reorder_by_stable_id() {
        let mut state = CrossWorkspacePaneState::default();
        let first = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "first"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let second = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "second"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        assert!(state.focus_attachment(first));

        assert!(state.reorder(second, 0));

        assert_eq!(state.focused(), FocusedSurface::Attached(first));
        assert_eq!(state.attachments()[0].id(), second);
        assert_eq!(state.attachments()[1].id(), first);
    }

    #[test]
    fn reorder_never_moves_primary_surface() {
        let mut state = CrossWorkspacePaneState::default();
        let first = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "first"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let second = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "second"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        state.focus_primary();

        assert!(state.reorder(second, 0));
        assert_eq!(state.focused(), FocusedSurface::Primary);
        assert_eq!(state.attachments()[0].id(), second);
        assert_eq!(state.attachments()[1].id(), first);
    }

    #[test]
    fn capacity_trim_detaches_rightmost_views() {
        let mut state = CrossWorkspacePaneState::default();
        let targets = [
            distinct_target("workspace-b", 1, "first"),
            distinct_target("workspace-c", 2, "second"),
            distinct_target("workspace-d", 3, "third"),
        ];
        for target in targets.iter().cloned() {
            assert!(matches!(
                state.attach_right("workspace-a", target, 420.0, 6),
                AttachOutcome::Appended(_)
            ));
        }

        let detached = state.enforce_capacity(1);

        assert_eq!(state.attachments().len(), 1);
        assert_eq!(state.attachments()[0].target(), &targets[0]);
        assert_eq!(
            detached
                .iter()
                .map(DetachDecision::target)
                .collect::<Vec<_>>(),
            vec![&targets[2], &targets[1]]
        );
        assert_eq!(state.focused(), FocusedSurface::Primary);

        for suffix in 0..HARD_MAX_CROSS_WORKSPACE_PANES {
            let _ = state.attach_right(
                "workspace-a",
                distinct_target("workspace-z", 9, &format!("hard-{suffix}")),
                420.0,
                usize::MAX,
            );
        }
        assert_eq!(state.attachments().len(), HARD_MAX_CROSS_WORKSPACE_PANES);
        assert_eq!(
            state.attach_right(
                "workspace-a",
                distinct_target("workspace-z", 9, "overflow"),
                420.0,
                usize::MAX,
            ),
            AttachOutcome::CapacityReached
        );
    }

    #[test]
    fn detach_last_reference_releases_runtime_protection() {
        let mut state = CrossWorkspacePaneState::default();
        let first = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 7, "first"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let second = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 7, "second"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();

        assert!(state.detach_attachment(first).is_some());
        assert!(state.protects_runtime("workspace-b", 7));
        assert!(state.detach_attachment(second).is_some());
        assert!(!state.protects_runtime("workspace-b", 7));
    }

    #[test]
    fn non_finite_width_uses_or_retains_safe_value() {
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut state = CrossWorkspacePaneState::default();
            let id = state
                .attach_right(
                    "workspace-a",
                    distinct_target("workspace-b", 1, "width"),
                    invalid,
                    6,
                )
                .appended_id()
                .unwrap();
            assert_eq!(state.attachments()[0].width_px(), 420.0);

            assert!(state.set_width(id, 200.0));
            assert_eq!(state.attachments()[0].width_px(), 320.0);
            assert!(state.set_width(id, 2_000.0));
            assert_eq!(state.attachments()[0].width_px(), 960.0);
            assert!(!state.set_width(id, invalid));
            assert_eq!(state.attachments()[0].width_px(), 960.0);
        }
    }

    #[test]
    fn reconcile_one_target_does_not_mutate_siblings() {
        let mut state = CrossWorkspacePaneState::default();
        let first = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "first"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let second = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "second"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();

        assert_eq!(
            state.reconcile_target(
                "workspace-a",
                first,
                AttachedRuntimeState::Live {
                    runtime_instance: 1,
                    target_relation: LiveTargetRelation::PaneMissing,
                },
            ),
            ReconcileDecision::Detached(DetachReason::PaneMissing)
        );

        assert_eq!(state.attachments().len(), 1);
        assert_eq!(state.attachments()[0].id(), second);
        assert_eq!(
            state.render_state_for(second),
            Some(AttachedRenderState::Live)
        );
        assert_eq!(state.focused(), FocusedSurface::Attached(second));
    }

    #[test]
    fn restoring_placeholder_reserves_ordered_slot_and_deduplicates() {
        let mut state = CrossWorkspacePaneState::default();
        let request = PersistedPaneRequest::new("workspace-b", MuxPaneId("pane-cold".into()));

        let AttachOutcome::Appended(id) =
            state.append_restoring("workspace-a", request.clone(), 420.0, 6)
        else {
            panic!("restoring request should reserve a slot");
        };

        assert_eq!(state.attachments()[0].id(), id);
        assert_eq!(state.attachments()[0].restoring_request(), Some(&request));
        assert_eq!(state.focused(), FocusedSurface::Attached(id));
        assert!(!state.protects_runtime("workspace-b", 1));
        assert_eq!(
            state.append_restoring("workspace-a", request, 700.0, 6),
            AttachOutcome::FocusedExisting(id)
        );
        assert_eq!(state.attachments().len(), 1);
    }

    #[test]
    fn promotion_requires_exact_persisted_workspace_and_pane() {
        let mut state = CrossWorkspacePaneState::default();
        let request = PersistedPaneRequest::new("workspace-b", MuxPaneId("pane-cold".into()));
        let id = state
            .append_restoring("workspace-a", request.clone(), 420.0, 6)
            .appended_id()
            .unwrap();
        let wrong = distinct_target("workspace-b", 7, "wrong");

        assert!(!state.promote_restoring(id, wrong));
        assert_eq!(state.attachments()[0].restoring_request(), Some(&request));

        let exact = WorkspacePaneTarget::new(
            "workspace-b",
            7,
            MuxTabId("tab-cold".into()),
            MuxPaneId("pane-cold".into()),
            SessionId(77),
        );
        assert!(state.promote_restoring(id, exact.clone()));
        assert_eq!(state.attachments()[0].id(), id);
        assert_eq!(state.attachments()[0].target(), &exact);
        assert_eq!(state.focused(), FocusedSurface::Attached(id));
        assert!(state.protects_runtime("workspace-b", 7));
    }

    #[test]
    fn canonical_pane_deduplicates_across_live_and_restoring_sources() {
        let request = PersistedPaneRequest::new("workspace-b", MuxPaneId("pane-shared".into()));
        let live = WorkspacePaneTarget::new(
            "workspace-b",
            7,
            MuxTabId("tab-shared".into()),
            MuxPaneId("pane-shared".into()),
            SessionId(77),
        );

        let mut live_first = CrossWorkspacePaneState::default();
        let live_id = live_first
            .attach_right("workspace-a", live.clone(), 420.0, 6)
            .appended_id()
            .unwrap();
        assert_eq!(
            live_first.append_restoring("workspace-a", request.clone(), 420.0, 6),
            AttachOutcome::FocusedExisting(live_id)
        );
        assert_eq!(live_first.attachments().len(), 1);

        let mut restoring_first = CrossWorkspacePaneState::default();
        let restoring_id = restoring_first
            .append_restoring("workspace-a", request, 420.0, 6)
            .appended_id()
            .unwrap();
        assert_eq!(
            restoring_first.attach_right("workspace-a", live, 420.0, 6),
            AttachOutcome::FocusedExisting(restoring_id)
        );
        assert_eq!(restoring_first.attachments().len(), 1);
    }
}
