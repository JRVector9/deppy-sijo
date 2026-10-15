extern crate self as deppy_core;
extern crate self as runtime;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MuxPaneId(pub String);
mod ui {
    pub mod diff_viewer { #[derive(Default)] pub struct DiffViewerUi; }
    pub mod git_panel { #[derive(Default)] pub struct GitPanelUi; }
    pub mod work_history { #[derive(Default)] pub struct WorkHistoryUi; }
    pub mod transcript_viewer { #[derive(Default)] pub struct TranscriptViewerUi; }
    pub mod aux_search {
        #[derive(Default)] pub struct AuxSearchState;
        impl AuxSearchState { pub fn reset(&mut self) {} }
    }
    pub mod workspace {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct DocumentTabId(pub u32);
        #[derive(Default, Clone, Copy)] pub enum PaneAuxTabState { #[default] Closed, OpenActive }
        impl PaneAuxTabState { pub fn on_close(self) -> Self { Self::Closed } }
    }
}
#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/app/src/session_aux.rs"]
mod session_aux;

struct OpenDocument { id: ui::workspace::DocumentTabId, path: std::path::PathBuf, source: String, dirty: bool }
// The production helper below is copied without change; caller filter is identical to
// App::begin_document_open: self.session_aux.current.document_ids.contains(&d.id).
fn find_open_document_by_path<'a>(
    documents: impl IntoIterator<Item = &'a OpenDocument>,
    path: &std::path::Path,
) -> Option<ui::workspace::DocumentTabId> {
    documents
        .into_iter()
        .find(|document| document.path == path)
        .map(|document| document.id)
}
fn main() {
    let old = session_aux::SessionAuxScope { workspace_id: "ws".into(), runtime_instance: 1, pane: Some(MuxPaneId("closed-pane".into())), session: Some(SessionId(1)) };
    let new = session_aux::SessionAuxScope { workspace_id: "ws".into(), runtime_instance: 1, pane: Some(MuxPaneId("new-pane".into())), session: Some(SessionId(2)) };
    let id=ui::workspace::DocumentTabId(1);
    let documents=vec![OpenDocument { id, path: "/private/tmp/fictional-dirty-document.md".into(), source: "unsaved edit".into(), dirty: true }];
    let mut views=session_aux::SessionAuxViews::default();
    views.switch(old.clone());
    views.current.document_ids.push(id);
    views.current.active_document=Some(id);
    views.current.document_tab=ui::workspace::PaneAuxTabState::OpenActive;
    // Runtime ClosePane removes the old pane; actual App sync chooses a live different owner.
    views.switch(new.clone());
    views.retain_live(|scope| scope == &new);
    let reopening = find_open_document_by_path(documents.iter().filter(|d|views.current.document_ids.contains(&d.id)), &documents[0].path);
    println!("after_closed_owner: visible_document_ids={:?}; reopen_same_path={reopening:?}; retained_dirty={}; retained_source={}",views.current.document_ids,documents[0].dirty,documents[0].source);
    // Show preservation is in an owner that no longer has a pane to select in actual mux.
    views.switch(old);
    println!("only_after_explicit_dead_scope_switch={:?}",views.current.document_ids);
    assert_eq!(reopening,Some(id),"explicit reopening from a live pane must recover the retained dirty document instead of creating another buffer");
}
