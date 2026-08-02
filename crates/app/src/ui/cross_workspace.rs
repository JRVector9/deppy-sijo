use runtime::{MuxPaneId, MuxTabId, SessionId};

pub(crate) const MIN_ATTACHED_WIDTH_PX: f32 = 320.0;
pub(crate) const MAX_ATTACHED_WIDTH_PX: f32 = 960.0;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertAnchor {
    #[allow(dead_code)]
    Primary,
    #[allow(dead_code)]
    Attached(AttachmentId),
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertAnchorError {
    StaleAttachment(AttachmentId),
}

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
pub(crate) enum AttachedPaneSource {
    Restoring(PersistedPaneRequest),
    Live(WorkspacePaneTarget),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachedPane {
    id: AttachmentId,
    primary_workspace_id: String,
    source: AttachedPaneSource,
    width_px: f32,
    render_state: AttachedRenderState,
}

impl AttachedPane {
    pub(crate) fn id(&self) -> AttachmentId {
        self.id
    }

    pub(crate) fn source(&self) -> &AttachedPaneSource {
        &self.source
    }

    pub(crate) fn workspace_id(&self) -> &str {
        match &self.source {
            AttachedPaneSource::Restoring(request) => &request.workspace_id,
            AttachedPaneSource::Live(target) => &target.workspace_id,
        }
    }

    pub(crate) fn live_target(&self) -> Option<&WorkspacePaneTarget> {
        match &self.source {
            AttachedPaneSource::Live(target) => Some(target),
            AttachedPaneSource::Restoring(_) => None,
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
    pub(crate) fn attach_right(
        &mut self,
        primary_workspace_id: impl Into<String>,
        target: WorkspacePaneTarget,
        width_px: f32,
        limit: usize,
    ) -> AttachOutcome {
        self.attach_at_anchor(
            primary_workspace_id,
            target,
            width_px,
            limit,
            InsertAnchor::End,
        )
        .expect("end anchor is always valid")
    }

    pub(crate) fn attach_at_anchor(
        &mut self,
        primary_workspace_id: impl Into<String>,
        target: WorkspacePaneTarget,
        width_px: f32,
        limit: usize,
        anchor: InsertAnchor,
    ) -> Result<AttachOutcome, InsertAnchorError> {
        let insertion_index = self.insertion_index(anchor)?;
        if let Some(index) =
            self.attachments
                .iter()
                .position(|attachment| match &attachment.source {
                    AttachedPaneSource::Restoring(request) => {
                        request.workspace_id == target.workspace_id && request.pane == target.pane
                    }
                    AttachedPaneSource::Live(existing) => existing == &target,
                })
        {
            let attachment = &mut self.attachments[index];
            if matches!(attachment.source, AttachedPaneSource::Restoring(_)) {
                attachment.source = AttachedPaneSource::Live(target);
                attachment.render_state = AttachedRenderState::Live;
            }
            self.focused = FocusedSurface::Attached(attachment.id);
            return Ok(AttachOutcome::FocusedExisting(attachment.id));
        }
        if self.attachments.len() >= limit.min(HARD_MAX_CROSS_WORKSPACE_PANES) {
            return Ok(AttachOutcome::CapacityReached);
        }

        let id = self.allocate_id();
        self.attachments.insert(
            insertion_index,
            AttachedPane {
                id,
                primary_workspace_id: primary_workspace_id.into(),
                source: AttachedPaneSource::Live(target),
                width_px: normalize_width(width_px),
                render_state: AttachedRenderState::Live,
            },
        );
        self.focused = FocusedSurface::Attached(id);
        Ok(AttachOutcome::Appended(id))
    }

    pub(crate) fn append_restoring(
        &mut self,
        primary_workspace_id: impl Into<String>,
        request: PersistedPaneRequest,
        width_px: f32,
        limit: usize,
    ) -> AttachOutcome {
        self.insert_restoring_at_anchor(
            primary_workspace_id,
            request,
            width_px,
            limit,
            InsertAnchor::End,
        )
        .expect("end anchor is always valid")
    }

    pub(crate) fn insert_restoring_at_anchor(
        &mut self,
        primary_workspace_id: impl Into<String>,
        request: PersistedPaneRequest,
        width_px: f32,
        limit: usize,
        anchor: InsertAnchor,
    ) -> Result<AttachOutcome, InsertAnchorError> {
        let insertion_index = self.insertion_index(anchor)?;
        if let Some(existing) = self.attachments.iter().find(|attachment| {
            attachment.matches_canonical_pane(&request.workspace_id, &request.pane)
        }) {
            self.focused = FocusedSurface::Attached(existing.id);
            return Ok(AttachOutcome::FocusedExisting(existing.id));
        }
        if self.attachments.len() >= limit.min(HARD_MAX_CROSS_WORKSPACE_PANES) {
            return Ok(AttachOutcome::CapacityReached);
        }

        let id = self.allocate_id();
        self.attachments.insert(
            insertion_index,
            AttachedPane {
                id,
                primary_workspace_id: primary_workspace_id.into(),
                source: AttachedPaneSource::Restoring(request),
                width_px: normalize_width(width_px),
                render_state: AttachedRenderState::Placeholder(AttachedPlaceholder::Disconnected),
            },
        );
        self.focused = FocusedSurface::Attached(id);
        Ok(AttachOutcome::Appended(id))
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

    pub(crate) fn attachments(&self) -> &[AttachedPane] {
        &self.attachments
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

    pub(crate) fn resize_adjacent(
        &mut self,
        left_id: AttachmentId,
        right_id: AttachmentId,
        requested_left_width: f32,
    ) -> bool {
        if !requested_left_width.is_finite() {
            return false;
        }
        let Some(left_index) = self
            .attachments
            .iter()
            .position(|attachment| attachment.id == left_id)
        else {
            return false;
        };
        if self
            .attachments
            .get(left_index + 1)
            .map(|attachment| attachment.id)
            != Some(right_id)
        {
            return false;
        }

        let (before_right, from_right) = self.attachments.split_at_mut(left_index + 1);
        let left = &mut before_right[left_index];
        let right = &mut from_right[0];
        let pair_width = left.width_px + right.width_px;
        let minimum_left = MIN_ATTACHED_WIDTH_PX.max(pair_width - MAX_ATTACHED_WIDTH_PX);
        let maximum_left = MAX_ATTACHED_WIDTH_PX.min(pair_width - MIN_ATTACHED_WIDTH_PX);
        let next_left = requested_left_width.clamp(minimum_left, maximum_left);
        left.width_px = next_left;
        right.width_px = pair_width - next_left;
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

    pub(crate) fn render_state_for(&self, id: AttachmentId) -> Option<AttachedRenderState> {
        self.attachments
            .iter()
            .find(|attachment| attachment.id == id)
            .map(|attachment| attachment.render_state)
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

    fn insertion_index(&self, anchor: InsertAnchor) -> Result<usize, InsertAnchorError> {
        match anchor {
            InsertAnchor::Primary => Ok(0),
            InsertAnchor::Attached(id) => self
                .attachments
                .iter()
                .position(|attachment| attachment.id == id)
                .map(|index| index + 1)
                .ok_or(InsertAnchorError::StaleAttachment(id)),
            InsertAnchor::End => Ok(self.attachments.len()),
        }
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

    #[test]
    fn target_namespace_includes_workspace_and_runtime_instance() {
        let a = target("workspace-a", 1);
        let b = target("workspace-b", 1);
        let replacement = target("workspace-a", 2);

        assert_ne!(a, b);
        assert_ne!(a, replacement);
    }

    #[test]
    fn attached_runtime_is_protected_from_warm_eviction() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach_right("workspace-a", target("workspace-b", 9), 420.0, 6);

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
                .map(|attachment| attachment.live_target().unwrap())
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
    fn cross_workspace_insert_anchor_orders_primary_attached_and_end() {
        let mut state = CrossWorkspacePaneState::default();
        let targets = [
            distinct_target("workspace-b", 1, "first"),
            distinct_target("workspace-c", 2, "second"),
            distinct_target("workspace-d", 3, "primary"),
            distinct_target("workspace-e", 4, "middle"),
            distinct_target("workspace-f", 5, "end"),
        ];
        let first_id = state
            .attach_right("workspace-a", targets[0].clone(), 420.0, 6)
            .appended_id()
            .unwrap();
        state.attach_right("workspace-a", targets[1].clone(), 420.0, 6);
        state
            .attach_at_anchor(
                "workspace-a",
                targets[2].clone(),
                420.0,
                6,
                InsertAnchor::Primary,
            )
            .unwrap();
        state
            .attach_at_anchor(
                "workspace-a",
                targets[3].clone(),
                420.0,
                6,
                InsertAnchor::Attached(first_id),
            )
            .unwrap();
        state
            .attach_at_anchor(
                "workspace-a",
                targets[4].clone(),
                420.0,
                6,
                InsertAnchor::End,
            )
            .unwrap();

        let order = state
            .attachments()
            .iter()
            .map(|attachment| attachment.live_target().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                &targets[2],
                &targets[0],
                &targets[3],
                &targets[1],
                &targets[4]
            ]
        );
    }

    #[test]
    fn cross_workspace_insert_anchor_rejections_and_duplicates_do_not_reorder() {
        let mut state = CrossWorkspacePaneState::default();
        let first = distinct_target("workspace-b", 1, "first");
        let second = distinct_target("workspace-c", 2, "second");
        let first_id = state
            .attach_right("workspace-a", first.clone(), 420.0, 2)
            .appended_id()
            .unwrap();
        let second_id = state
            .attach_right("workspace-a", second, 420.0, 2)
            .appended_id()
            .unwrap();
        let stale_id = AttachmentId(second_id.0 + 100);
        state.focus_primary();
        let attachments = state.attachments().to_vec();
        let focus = state.focused();

        assert_eq!(
            state.attach_at_anchor(
                "workspace-a",
                distinct_target("workspace-d", 3, "stale"),
                420.0,
                2,
                InsertAnchor::Attached(stale_id),
            ),
            Err(InsertAnchorError::StaleAttachment(stale_id))
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), focus);
        assert_eq!(
            state.attach_at_anchor(
                "workspace-a",
                first.clone(),
                420.0,
                2,
                InsertAnchor::Attached(stale_id),
            ),
            Err(InsertAnchorError::StaleAttachment(stale_id))
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), focus);
        assert_eq!(
            state.attach_at_anchor("workspace-a", first, 420.0, 2, InsertAnchor::Primary,),
            Ok(AttachOutcome::FocusedExisting(first_id))
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), FocusedSurface::Attached(first_id));
        state.focus_primary();
        assert_eq!(
            state.attach_at_anchor(
                "workspace-a",
                distinct_target("workspace-e", 4, "capacity"),
                420.0,
                2,
                InsertAnchor::Primary,
            ),
            Ok(AttachOutcome::CapacityReached)
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), focus);
    }

    #[test]
    fn cross_workspace_insert_anchor_restoring_inserts_and_fails_closed() {
        let mut state = CrossWorkspacePaneState::default();
        let first_id = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "first"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        state.attach_right(
            "workspace-a",
            distinct_target("workspace-c", 2, "second"),
            420.0,
            6,
        );
        let middle_request =
            PersistedPaneRequest::new("workspace-d", MuxPaneId("pane-middle".into()));
        state
            .insert_restoring_at_anchor(
                "workspace-a",
                middle_request.clone(),
                420.0,
                6,
                InsertAnchor::Attached(first_id),
            )
            .unwrap();
        state
            .insert_restoring_at_anchor(
                "workspace-a",
                PersistedPaneRequest::new("workspace-e", MuxPaneId("pane-end".into())),
                420.0,
                6,
                InsertAnchor::End,
            )
            .unwrap();
        state.focus_primary();
        let attachments = state.attachments().to_vec();
        let focus = state.focused();
        let stale_id = AttachmentId(first_id.0 + 100);

        assert_eq!(
            state
                .attachments()
                .iter()
                .map(AttachedPane::workspace_id)
                .collect::<Vec<_>>(),
            vec!["workspace-b", "workspace-d", "workspace-c", "workspace-e"]
        );
        assert_eq!(
            state.attach_at_anchor(
                "workspace-a",
                WorkspacePaneTarget::new(
                    "workspace-d",
                    9,
                    MuxTabId("tab-middle".into()),
                    MuxPaneId("pane-middle".into()),
                    SessionId(99),
                ),
                420.0,
                6,
                InsertAnchor::Attached(stale_id),
            ),
            Err(InsertAnchorError::StaleAttachment(stale_id))
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), focus);
        assert_eq!(
            state.insert_restoring_at_anchor(
                "workspace-a",
                middle_request,
                420.0,
                6,
                InsertAnchor::Attached(stale_id),
            ),
            Err(InsertAnchorError::StaleAttachment(stale_id))
        );
        assert_eq!(state.attachments(), attachments);
        assert_eq!(state.focused(), focus);
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
        assert_eq!(state.attachments()[0].live_target(), Some(&targets[0]));
        assert_eq!(
            detached
                .iter()
                .map(|decision| decision.live_target().unwrap())
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
    fn adjacent_resize_preserves_pair_total_and_bounds() {
        let mut state = CrossWorkspacePaneState::default();
        let left = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "left"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let right = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "right"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();

        assert!(state.resize_adjacent(left, right, 500.0));
        assert_eq!(state.attachments()[0].width_px(), 500.0);
        assert_eq!(state.attachments()[1].width_px(), 340.0);

        assert!(state.resize_adjacent(left, right, 700.0));
        assert_eq!(state.attachments()[0].width_px(), 520.0);
        assert_eq!(state.attachments()[1].width_px(), 320.0);

        assert!(state.resize_adjacent(left, right, 100.0));
        assert_eq!(state.attachments()[0].width_px(), 320.0);
        assert_eq!(state.attachments()[1].width_px(), 520.0);
    }

    #[test]
    fn adjacent_resize_respects_maximum_width_for_both_panes() {
        let mut state = CrossWorkspacePaneState::default();
        let left = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "left"),
                900.0,
                6,
            )
            .appended_id()
            .unwrap();
        let right = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "right"),
                400.0,
                6,
            )
            .appended_id()
            .unwrap();

        assert!(state.resize_adjacent(left, right, 200.0));
        assert_eq!(state.attachments()[0].width_px(), 340.0);
        assert_eq!(state.attachments()[1].width_px(), 960.0);

        assert!(state.resize_adjacent(left, right, 1_200.0));
        assert_eq!(state.attachments()[0].width_px(), 960.0);
        assert_eq!(state.attachments()[1].width_px(), 340.0);
    }

    #[test]
    fn adjacent_resize_rejects_invalid_pairs_without_mutation() {
        let mut state = CrossWorkspacePaneState::default();
        let left = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-b", 1, "left"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let middle = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-c", 2, "middle"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let right = state
            .attach_right(
                "workspace-a",
                distinct_target("workspace-d", 3, "right"),
                420.0,
                6,
            )
            .appended_id()
            .unwrap();
        let unknown = AttachmentId(right.0 + 100);
        let original = state.attachments().to_vec();

        for (candidate_left, candidate_right, requested_width) in [
            (unknown, middle, 500.0),
            (left, unknown, 500.0),
            (middle, left, 500.0),
            (left, right, 500.0),
            (left, middle, f32::NAN),
            (left, middle, f32::INFINITY),
            (left, middle, f32::NEG_INFINITY),
        ] {
            assert!(!state.resize_adjacent(candidate_left, candidate_right, requested_width));
            assert_eq!(state.attachments(), original);
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
        assert_eq!(state.attachments()[0].workspace_id(), "workspace-b");
        assert_eq!(state.attachments()[0].live_target(), None);
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
        assert_eq!(state.attachments()[0].workspace_id(), "workspace-b");
        assert_eq!(state.attachments()[0].live_target(), None);

        let exact = WorkspacePaneTarget::new(
            "workspace-b",
            7,
            MuxTabId("tab-cold".into()),
            MuxPaneId("pane-cold".into()),
            SessionId(77),
        );
        assert!(state.promote_restoring(id, exact.clone()));
        assert_eq!(state.attachments()[0].id(), id);
        assert_eq!(state.attachments()[0].live_target(), Some(&exact));
        assert_eq!(state.focused(), FocusedSurface::Attached(id));
        assert!(state.protects_runtime("workspace-b", 7));
    }

    #[test]
    fn attach_right_promotes_matching_restoring_slot_to_live() {
        let request = PersistedPaneRequest::new("workspace-b", MuxPaneId("pane-shared".into()));
        let live = WorkspacePaneTarget::new(
            "workspace-b",
            7,
            MuxTabId("tab-shared".into()),
            MuxPaneId("pane-shared".into()),
            SessionId(77),
        );

        let mut state = CrossWorkspacePaneState::default();
        let restoring_id = state
            .append_restoring("workspace-a", request, 420.0, 6)
            .appended_id()
            .unwrap();
        assert_eq!(
            state.attach_right("workspace-a", live.clone(), 420.0, 6),
            AttachOutcome::FocusedExisting(restoring_id)
        );
        assert_eq!(state.attachments().len(), 1);
        assert_eq!(state.attachments()[0].live_target(), Some(&live));
        assert_eq!(state.focused(), FocusedSurface::Attached(restoring_id));
        assert!(state.protects_runtime("workspace-b", 7));
    }

    #[test]
    fn live_duplicates_require_full_exact_target() {
        let mut state = CrossWorkspacePaneState::default();
        let first = WorkspacePaneTarget::new(
            "workspace-b",
            7,
            MuxTabId("tab-shared".into()),
            MuxPaneId("pane-shared".into()),
            SessionId(77),
        );
        let replacement = WorkspacePaneTarget::new(
            "workspace-b",
            8,
            MuxTabId("tab-shared".into()),
            MuxPaneId("pane-shared".into()),
            SessionId(88),
        );
        let first_id = state
            .attach_right("workspace-a", first.clone(), 420.0, 6)
            .appended_id()
            .unwrap();

        let AttachOutcome::Appended(replacement_id) =
            state.attach_right("workspace-a", replacement.clone(), 420.0, 6)
        else {
            panic!("different runtime/session target must append");
        };

        assert_ne!(first_id, replacement_id);
        assert_eq!(state.attachments().len(), 2);
        assert_eq!(state.attachments()[0].live_target(), Some(&first));
        assert_eq!(state.attachments()[1].live_target(), Some(&replacement));
    }

    #[test]
    fn capacity_trim_of_restoring_slot_returns_explicit_source() {
        let mut state = CrossWorkspacePaneState::default();
        let live = distinct_target("workspace-b", 7, "live");
        let request = PersistedPaneRequest::new("workspace-c", MuxPaneId("pane-cold".into()));
        state.attach_right("workspace-a", live.clone(), 420.0, 6);
        state.append_restoring("workspace-a", request.clone(), 420.0, 6);

        let mut detached = state.enforce_capacity(1);
        let decision = detached.pop().unwrap();

        assert_eq!(decision.live_target(), None);
        assert_eq!(decision.restoring_request(), Some(&request));
        assert_eq!(state.attachments().len(), 1);
        assert_eq!(state.attachments()[0].live_target(), Some(&live));
        assert!(state.protects_runtime("workspace-b", 7));
        assert!(!state.protects_runtime("workspace-c", 8));
    }

    #[test]
    fn cancel_restoring_slot_returns_explicit_request() {
        let mut state = CrossWorkspacePaneState::default();
        let request = PersistedPaneRequest::new("workspace-b", MuxPaneId("pane-cold".into()));
        let restoring_id = state
            .append_restoring("workspace-a", request.clone(), 420.0, 6)
            .appended_id()
            .unwrap();

        let decision = state.detach_attachment(restoring_id).unwrap();

        assert_eq!(decision.live_target(), None);
        assert_eq!(decision.restoring_request(), Some(&request));
        assert!(state.attachments().is_empty());
    }

    #[test]
    fn attachment_workspace_id_is_exact_for_live_and_restoring_sources() {
        let mut state = CrossWorkspacePaneState::default();
        state.attach_right(
            "workspace-a",
            distinct_target("workspace-b", 7, "live"),
            420.0,
            6,
        );
        state.append_restoring(
            "workspace-a",
            PersistedPaneRequest::new("workspace-c", MuxPaneId("pane-cold".into())),
            420.0,
            6,
        );

        assert_eq!(state.attachments()[0].workspace_id(), "workspace-b");
        assert_eq!(state.attachments()[1].workspace_id(), "workspace-c");
    }
}
