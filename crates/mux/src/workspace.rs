use std::path::PathBuf;

use deppy_core::{MuxWindowId, WorkspaceId};

/// 설계문서 5.2 MuxWorkspace.
pub struct MuxWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root_path: PathBuf,
    pub windows: Vec<MuxWindowId>,
}

impl MuxWorkspace {
    pub fn new(id: WorkspaceId, name: String, root_path: PathBuf) -> Self {
        Self {
            id,
            name,
            root_path,
            windows: Vec::new(),
        }
    }
}
