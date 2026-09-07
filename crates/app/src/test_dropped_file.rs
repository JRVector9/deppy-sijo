//! Native OS-drop fixtures: routing reads paths and leaves file I/O to the host.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
struct PathOnlyFile(PathBuf);

impl egui::DroppedFile for PathOnlyFile {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        panic!("OS-drop routing must not read file bytes in the UI");
    }
}

pub(crate) fn handle(path: PathBuf) -> egui::DroppedFileHandle {
    Arc::new(PathOnlyFile(path))
}
