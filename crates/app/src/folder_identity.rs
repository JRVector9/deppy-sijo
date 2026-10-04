//! Filesystem identity probing for the settings worker; never called by rendering.
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FolderIdentity {
    pub anchor: storage::WorkspaceFolderAnchor,
    pub volume: Option<uuid::Uuid>,
}

#[cfg(unix)]
pub(crate) fn probe(path: &Path) -> Option<FolderIdentity> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // A replaced selection may be a FIFO/device. Reject it during open rather
    // than blocking the settings worker before metadata can check is_dir().
    let folder = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = folder.metadata().ok()?;
    if !metadata.is_dir() {
        return None;
    }
    let anchor = storage::WorkspaceFolderAnchor {
        dev: metadata.dev() as i64,
        ino: metadata.ino() as i64,
    };
    let volume = volume_uuid(&folder);
    // The descriptor binds UUID and inode to one object. Reject a path replaced
    // between opening it and returning its identity to the registration worker.
    let current = std::fs::metadata(path).ok()?;
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return None;
    }
    Some(FolderIdentity { anchor, volume })
}

#[cfg(not(unix))]
pub(crate) fn probe(_path: &Path) -> Option<FolderIdentity> {
    None
}

#[cfg(target_os = "macos")]
fn volume_uuid(folder: &std::fs::File) -> Option<uuid::Uuid> {
    use std::os::fd::AsRawFd;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist attributes have four-byte alignment: length then uuid_t[16].
    let mut buffer = [0_u32; 5];
    // SAFETY: the owned directory fd remains alive, attributes is initialized,
    // and the writable buffer has the requested20 bytes and four-byte alignment.
    let status = unsafe {
        libc::fgetattrlist(
            folder.as_raw_fd(),
            std::ptr::from_mut(&mut attributes).cast(),
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer),
            0,
        )
    };
    if status != 0 || buffer[0] != 20 {
        return None;
    }
    let mut bytes = [0_u8; 16];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(&buffer[1..]) {
        chunk.copy_from_slice(&word.to_ne_bytes());
    }
    let id = uuid::Uuid::from_bytes(bytes);
    (!id.is_nil()).then_some(id)
}

#[cfg(not(target_os = "macos"))]
fn volume_uuid(_folder: &std::fs::File) -> Option<uuid::Uuid> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn popup_review_fifo_is_rejected_without_waiting_for_a_writer() {
        use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
        let root = std::env::temp_dir().join(format!("deppy-volume-fifo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let fifo = root.join("replaced-folder");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated temp path; mkfifo creates no open handle.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        let handle = std::thread::spawn(move || {
            tx.send(probe(&path)).unwrap();
        });
        let timely = rx.recv_timeout(std::time::Duration::from_secs(1));
        if timely.is_err() {
            // Unblock the old implementation before failing, so RED leaves no
            // live reader/thread. A directory-only probe never needs a writer.
            let _writer = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
                .unwrap();
            assert!(
                rx.recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
                    .is_none()
            );
        }
        handle.join().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(
            matches!(timely, Ok(None)),
            "probe must reject FIFO immediately"
        );
    }

    #[test]
    fn popup_review_real_directory_identity_is_stable_and_rejects_files() {
        let root =
            std::env::temp_dir().join(format!("deppy-volume-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("child")).unwrap();
        std::fs::write(root.join("file"), "test").unwrap();
        let first = probe(&root).unwrap();
        let second = probe(&root).unwrap();
        let child = probe(&root.join("child")).unwrap();
        let file = probe(&root.join("file"));
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(first.anchor, second.anchor);
        assert_eq!(first.volume, second.volume);
        assert_ne!(first.anchor.ino, child.anchor.ino);
        assert_eq!(first.volume, child.volume);
        assert!(file.is_none());
        #[cfg(target_os = "macos")]
        assert!(
            first.volume.is_some(),
            "native UUID decoding should work on the macOS test volume"
        );
    }
}
