//! UI-only auxiliary views. Buffers and IO remain globally bounded, keyed by document/request id.
use crate::ui;
use deppy_core::SessionId;
use runtime::MuxPaneId;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionAuxScope {
    pub workspace_id: String,
    pub runtime_instance: u64,
    pub pane: Option<MuxPaneId>,
    pub session: Option<SessionId>,
}

#[derive(Default)]
pub(crate) struct SessionAuxView {
    pub diff_viewer_ui: ui::diff_viewer::DiffViewerUi,
    pub git_panel_ui: ui::git_panel::GitPanelUi,
    pub git_panel_cwd: Option<PathBuf>,
    pub git_tab: ui::workspace::PaneAuxTabState,
    pub git_tab_split_width: Option<f32>,
    pub git_generation: Option<u64>,
    pub git_diff_generation: Option<u64>,
    pub git_commit_files_generation: Option<u64>,
    pub work_history_ui: ui::work_history::WorkHistoryUi,
    pub work_history_tab: ui::workspace::PaneAuxTabState,
    pub work_history_tab_split_width: Option<f32>,
    pub document_tab: ui::workspace::PaneAuxTabState,
    pub document_ids: Vec<ui::workspace::DocumentTabId>,
    pub active_document: Option<ui::workspace::DocumentTabId>,
    pub document_tab_split_width: Option<f32>,
    pub document_cap_notice: bool,
    pub transcript_viewer_ui: ui::transcript_viewer::TranscriptViewerUi,
    pub transcript_generation: Option<u64>,
    pub aux_search: ui::aux_search::AuxSearchState,
}
impl SessionAuxView {
    /// Snapshot, file diff and commit files keep separate slots so one kind never drops another.
    fn owns_git_generation(&self, generation: u64) -> bool {
        [
            self.git_generation,
            self.git_diff_generation,
            self.git_commit_files_generation,
        ]
        .contains(&Some(generation))
    }
    fn close_document(&mut self, id: ui::workspace::DocumentTabId) {
        let Some(index) = self
            .document_ids
            .iter()
            .position(|candidate| *candidate == id)
        else {
            return;
        };
        self.document_ids.remove(index);
        if self.active_document == Some(id) {
            self.active_document = self
                .document_ids
                .get(index.min(self.document_ids.len().saturating_sub(1)))
                .copied();
            self.aux_search.reset();
        }
        if self.document_ids.is_empty() {
            self.document_tab = self.document_tab.on_close();
        }
    }
}
#[derive(Default)]
pub(crate) struct SessionAuxViews {
    pub owner: Option<SessionAuxScope>,
    pub current: SessionAuxView,
    parked: HashMap<SessionAuxScope, SessionAuxView>,
    dead_document_owners: HashSet<SessionAuxScope>,
}
impl SessionAuxViews {
    pub fn switch(&mut self, owner: SessionAuxScope) {
        if self.owner.as_ref() == Some(&owner) {
            return;
        }
        let next = self.parked.remove(&owner).unwrap_or_default();
        let previous = std::mem::replace(&mut self.current, next);
        if let Some(old) = self.owner.replace(owner) {
            self.parked.insert(old, previous);
        }
    }
    pub fn close_document(&mut self, id: ui::workspace::DocumentTabId) {
        self.current.close_document(id);
        for view in self.parked.values_mut() {
            view.close_document(id);
        }
    }
    pub fn document_owners(&self) -> impl Iterator<Item = (&SessionAuxScope, &SessionAuxView)> {
        self.owner
            .as_ref()
            .map(|owner| (owner, &self.current))
            .into_iter()
            .chain(self.parked.iter())
    }
    pub fn document_has_dead_owner(&self, id: ui::workspace::DocumentTabId) -> bool {
        self.document_owners().any(|(scope, view)| {
            self.dead_document_owners.contains(scope) && view.document_ids.contains(&id)
        })
    }
    /// An explicit reopen transfers just this retained document. Merely switching
    /// sessions never imports another owner's tabs, and live owners are excluded.
    pub fn recover_document(&mut self, id: ui::workspace::DocumentTabId) -> bool {
        let old = self.parked.iter().find_map(|(scope, view)| {
            (self.dead_document_owners.contains(scope) && view.document_ids.contains(&id))
                .then(|| scope.clone())
        });
        let Some(old) = old else {
            return false;
        };
        self.parked.get_mut(&old).unwrap().close_document(id);
        if !self.current.document_ids.contains(&id) {
            self.current.document_ids.push(id);
        }
        self.current.active_document = Some(id);
        self.current.document_tab = ui::workspace::PaneAuxTabState::OpenActive;
        self.current.aux_search.reset();
        true
    }
    pub fn git_result_owner(&mut self, generation: u64) -> Option<&mut SessionAuxView> {
        if self.current.owns_git_generation(generation) {
            return Some(&mut self.current);
        }
        self.parked
            .values_mut()
            .find(|view| view.owns_git_generation(generation))
    }
    pub fn transcript_result_owner(&mut self, generation: u64) -> Option<&mut SessionAuxView> {
        if self.current.transcript_generation == Some(generation) {
            return Some(&mut self.current);
        }
        self.parked
            .values_mut()
            .find(|view| view.transcript_generation == Some(generation))
    }
    pub fn retain_live(&mut self, mut live: impl FnMut(&SessionAuxScope) -> bool) {
        self.dead_document_owners = self
            .document_owners()
            .filter(|(scope, view)| !view.document_ids.is_empty() && !live(scope))
            .map(|(scope, _)| scope.clone())
            .collect();
        // Retain dead-owner buffers for explicit reopen; do not implicitly share
        // them with a newly focused live session.
        self.parked
            .retain(|scope, view| !view.document_ids.is_empty() || live(scope));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn scope(runtime: u64, session: u64) -> SessionAuxScope {
        SessionAuxScope {
            workspace_id: "ws".into(),
            runtime_instance: runtime,
            pane: Some(MuxPaneId(format!("p{session}"))),
            session: Some(SessionId(session)),
        }
    }
    #[test]
    fn session_aux_switch_restores_views_and_routes_late_results() {
        let mut views = SessionAuxViews::default();
        views.switch(scope(1, 1));
        views.current.git_tab = ui::workspace::PaneAuxTabState::OpenInactive;
        views.current.document_tab = ui::workspace::PaneAuxTabState::OpenActive;
        views.current.document_ids = vec![
            ui::workspace::DocumentTabId(3),
            ui::workspace::DocumentTabId(4),
        ];
        views.current.active_document = Some(ui::workspace::DocumentTabId(3));
        views.current.git_generation = Some(8);
        views.current.git_diff_generation = Some(10);
        views.current.git_commit_files_generation = Some(11);
        views.current.transcript_generation = Some(9);
        views.switch(scope(1, 2));
        assert!(!views.current.git_tab.is_open());
        assert!(!views.current.document_tab.is_open());
        assert!(views.current.document_ids.is_empty());
        views.current.document_ids = vec![ui::workspace::DocumentTabId(5)];
        views.current.active_document = Some(ui::workspace::DocumentTabId(5));
        views.git_result_owner(8).unwrap().git_panel_cwd = Some(PathBuf::from("/a"));
        assert!(views.current.git_panel_cwd.is_none());
        assert!(views.transcript_result_owner(9).is_some());
        assert!(
            views.git_result_owner(10).is_some(),
            "diff must not cancel a pending snapshot"
        );
        assert!(
            views.git_result_owner(11).is_some(),
            "a file diff must not cancel a pending commit-files load"
        );
        assert!(views.git_result_owner(7).is_none());
        views.close_document(ui::workspace::DocumentTabId(3));
        assert_eq!(
            views.current.active_document,
            Some(ui::workspace::DocumentTabId(5))
        );
        views.switch(scope(1, 1));
        assert!(views.current.git_tab.is_open());
        assert_eq!(views.current.git_panel_cwd, Some(PathBuf::from("/a")));
        assert_eq!(
            views.current.active_document,
            Some(ui::workspace::DocumentTabId(4))
        );
        views.switch(scope(2, 1));
        assert!(
            !views.current.git_tab.is_open(),
            "runtime reuse must not inherit old UI"
        );
        assert!(views.current.document_ids.is_empty());
    }

    #[test]
    fn full_fix_20261014_document_explicit_recovery_excludes_live_owner() {
        let mut views = SessionAuxViews::default();
        let old = scope(1, 1);
        let current = scope(1, 2);
        let id = ui::workspace::DocumentTabId(7);
        views.switch(old.clone());
        views.current.document_ids = vec![id];
        views.current.active_document = Some(id);
        views.switch(current.clone());
        views.retain_live(|_| true);
        assert!(!views.document_has_dead_owner(id));
        assert!(!views.recover_document(id));
        assert!(
            views.current.document_ids.is_empty(),
            "live sessions must remain independent"
        );
        views.retain_live(|scope| scope == &current);
        assert!(views.document_has_dead_owner(id));
        assert!(views.recover_document(id));
        assert_eq!(views.current.document_ids, [id]);
        views.switch(old);
        assert!(
            views.current.document_ids.is_empty(),
            "recovery moves one owner instead of sharing the buffer"
        );
    }
    #[test]
    fn git_results_drop_only_when_the_same_kind_is_reissued() {
        let mut views = SessionAuxViews::default();
        views.switch(scope(1, 1));
        views.current.git_generation = Some(1);
        views.current.git_commit_files_generation = Some(2);
        // A newer snapshot supersedes only the older snapshot.
        views.current.git_generation = Some(3);
        assert!(views.git_result_owner(1).is_none());
        assert!(views.git_result_owner(2).is_some());
        assert!(views.git_result_owner(3).is_some());
    }
    #[test]
    fn session_aux_retains_live_or_open_documents_only() {
        let mut views = SessionAuxViews::default();
        views.switch(scope(1, 1));
        views.switch(scope(1, 2));
        views
            .current
            .document_ids
            .push(ui::workspace::DocumentTabId(1));
        views.switch(scope(1, 3));
        views.retain_live(|_| false);
        assert_eq!(views.parked.len(), 1);
        views.close_document(ui::workspace::DocumentTabId(1));
        views.retain_live(|_| false);
        assert!(views.parked.is_empty());
    }
}
