//! Local clipboard bytes are read on the host worker, then staged through the
//! existing authenticated SSH RPC lane. Only acknowledged remote paths reach PTYs.
use crate::ui::workspace::{TerminalClipboardPayload, WorkspaceIoErrorCode, WorkspaceIoOperation};
use deppy_core::workspace_target::{RemotePath, SshWorkspaceTarget};
use deppy_remote_host::{
    clipboard::{CHUNK_BYTES_MAX, IMAGE_BYTES_MAX, ImageChunk, Receipt},
    services::{Reply, Request, Response},
};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct Owner {
    pub workspace_id: String,
    pub runtime_instance: u64,
    pub target: SshWorkspaceTarget,
    pub operation: WorkspaceIoOperation,
    pub generation: u64,
}
pub(crate) enum ReadResult {
    Text(TerminalClipboardPayload),
    Images(Upload),
}
struct Image {
    id: String,
    data: Vec<u8>,
    extension: &'static str,
    digest: String,
}
pub(crate) struct Upload {
    images: VecDeque<Image>,
    offset: usize,
    paths: Vec<PathBuf>,
}

pub(crate) fn read() -> Result<ReadResult, WorkspaceIoErrorCode> {
    let paths = crate::ui::clipboard_image::paste_clipboard_paths_or_image_to_paths()
        .map_err(|_| WorkspaceIoErrorCode::NativeFailure)?
        .unwrap_or_default();
    read_with(paths, crate::ui::clipboard_image::read_clipboard_text)
}
fn read_with(
    paths: Vec<PathBuf>,
    text: impl FnOnce() -> Option<String>,
) -> Result<ReadResult, WorkspaceIoErrorCode> {
    if paths.is_empty() {
        return TerminalClipboardPayload::read_with(paths, text).map(ReadResult::Text);
    }
    if paths.len() > 16 {
        return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
    }
    let mut images = VecDeque::new();
    let mut total = 0usize;
    for path in paths {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| WorkspaceIoErrorCode::NativeFailure)?;
        let meta = file
            .metadata()
            .map_err(|_| WorkspaceIoErrorCode::NativeFailure)?;
        if !meta.is_file() {
            return Err(WorkspaceIoErrorCode::InvalidPath);
        }
        if meta.len() > IMAGE_BYTES_MAX as u64
            || meta.len() as usize > IMAGE_BYTES_MAX.saturating_sub(total)
        {
            return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
        }
        let mut data = Vec::new();
        (&mut file)
            .take((IMAGE_BYTES_MAX - total + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|_| WorkspaceIoErrorCode::NativeFailure)?;
        total += data.len();
        if total > IMAGE_BYTES_MAX {
            return Err(WorkspaceIoErrorCode::ClipboardTooLarge);
        }
        let extension = match image::guess_format(&data) {
            Ok(image::ImageFormat::Png) => "png",
            Ok(image::ImageFormat::Jpeg) => "jpg",
            Ok(image::ImageFormat::WebP) => "webp",
            Ok(image::ImageFormat::Gif) => "gif",
            _ => return Err(WorkspaceIoErrorCode::InvalidPath),
        };
        let digest = Sha256::digest(&data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        images.push_back(Image {
            id: uuid::Uuid::new_v4().to_string(),
            data,
            extension,
            digest,
        });
    }
    Ok(ReadResult::Images(Upload {
        images,
        offset: 0,
        paths: Vec::new(),
    }))
}
impl Upload {
    pub(crate) fn request(&self) -> Request {
        let image = self.images.front().expect("upload not complete");
        let end = (self.offset + CHUNK_BYTES_MAX).min(image.data.len());
        Request::StageClipboardImage(ImageChunk {
            upload_id: image.id.clone(),
            offset: self.offset as u64,
            total: image.data.len() as u64,
            sha256: image.digest.clone(),
            extension: image.extension.into(),
            data: image.data[self.offset..end].to_vec(),
        })
    }
    pub(crate) fn accept(
        &mut self,
        reply: Reply,
        remote_workspace_id: &str,
    ) -> Result<Option<TerminalClipboardPayload>, WorkspaceIoErrorCode> {
        let Ok(Response::ClipboardImage(Receipt { written, path })) = reply else {
            return Err(WorkspaceIoErrorCode::NativeFailure);
        };
        let image = self
            .images
            .front()
            .ok_or(WorkspaceIoErrorCode::NativeFailure)?;
        let end = (self.offset + CHUNK_BYTES_MAX).min(image.data.len());
        if written != end as u64 || path.is_some() != (end == image.data.len()) {
            return Err(WorkspaceIoErrorCode::NativeFailure);
        }
        if let Some(path) = &path {
            RemotePath::new(path.clone()).map_err(|_| WorkspaceIoErrorCode::InvalidPath)?;
            if remote_workspace_id.len() != 64
                || !remote_workspace_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(WorkspaceIoErrorCode::InvalidPath);
            }
            let expected_suffix = format!(
                "/.cache/deppy/ssh-clipboard/{}-{}.{}",
                &remote_workspace_id[..32],
                image.id,
                image.extension,
            );
            if !path.ends_with(&expected_suffix) {
                return Err(WorkspaceIoErrorCode::InvalidPath);
            }
        }
        self.offset = end;
        if let Some(path) = path {
            self.paths.push(PathBuf::from(path));
            self.images.pop_front();
            self.offset = 0;
        }
        if self.images.is_empty() {
            TerminalClipboardPayload::try_new(std::mem::take(&mut self.paths), None).map(Some)
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Files(PathBuf);
    impl Files {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("deppy-image-read-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn small_upload() -> Upload {
        Upload {
            images: VecDeque::from([Image {
                id: uuid::Uuid::new_v4().to_string(),
                data: b"\x89PNG\r\n\x1a\nfixture-image".to_vec(),
                extension: "png",
                digest: String::new(),
            }]),
            offset: 0,
            paths: Vec::new(),
        }
    }

    #[test]
    fn completed_receipt_must_match_the_staged_image() {
        for candidate in 0..7 {
            let mut upload = small_upload();
            let image = upload.images.front().unwrap();
            let written = image.data.len() as u64;
            let expected = format!(
                "/home/ubuntu/.cache/deppy/ssh-clipboard/{}-{}.png",
                &WORKSPACE[..32],
                image.id,
            );
            let path = match candidate {
                0 => "/".into(),
                1 => "/tmp/unrelated.png".into(),
                2 => "/Users/local/image.png".into(),
                3 => expected.replace(&image.id, &uuid::Uuid::new_v4().to_string()),
                4 => expected.replace(".png", ".jpg"),
                5 => expected.replace(&WORKSPACE[..32], &"b".repeat(32)),
                6 => expected.replace("/ssh-clipboard/", "/unrelated/"),
                _ => unreachable!(),
            };
            assert!(
                upload
                    .accept(
                        Ok(Response::ClipboardImage(Receipt {
                            written,
                            path: Some(path.clone()),
                        })),
                        WORKSPACE,
                    )
                    .is_err(),
                "accepted unrelated completed path: {path}"
            );
            assert_eq!(upload.offset, 0);
            assert!(upload.paths.is_empty());
        }
    }

    #[test]
    fn rejected_complete_path_does_not_advance_the_upload() {
        let mut upload = small_upload();
        let written = upload.images.front().unwrap().data.len() as u64;
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(Receipt {
                        written,
                        path: Some("relative.png".into()),
                    })),
                    WORKSPACE,
                )
                .is_err()
        );
        let Request::StageClipboardImage(next) = upload.request() else {
            panic!("typed staging request")
        };
        assert_eq!(next.offset, 0);
        assert_eq!(next.data.len() as u64, written);
    }

    #[test]
    fn malformed_workspace_ids_are_rejected_without_advancing() {
        for workspace in [String::new(), "a".into(), "g".repeat(64), "é".repeat(32)] {
            let mut upload = small_upload();
            let image = upload.images.front().unwrap();
            let receipt = Receipt {
                written: image.data.len() as u64,
                path: Some(format!(
                    "/home/ubuntu/.cache/deppy/ssh-clipboard/{}-{}.png",
                    &WORKSPACE[..32],
                    image.id,
                )),
            };
            assert!(matches!(
                upload.accept(Ok(Response::ClipboardImage(receipt)), &workspace),
                Err(WorkspaceIoErrorCode::InvalidPath)
            ));
            assert_eq!(upload.offset, 0);
        }
    }

    #[test]
    fn multiple_images_produce_one_payload_only_after_distinct_completions() {
        let mut upload = small_upload();
        upload.images.extend(small_upload().images);
        let mut previous_receipt = None;
        for index in 0..2 {
            let image = upload.images.front().unwrap();
            let receipt = Receipt {
                written: image.data.len() as u64,
                path: Some(format!(
                    "/home/ubuntu/.cache/deppy/ssh-clipboard/{}-{}.png",
                    &WORKSPACE[..32],
                    image.id,
                )),
            };
            if let Some(previous) = previous_receipt.take() {
                assert!(
                    upload
                        .accept(Ok(Response::ClipboardImage(previous)), WORKSPACE)
                        .is_err()
                );
                assert_eq!(upload.images.len(), 1);
                assert_eq!(upload.paths.len(), 1);
            }
            let payload = upload
                .accept(Ok(Response::ClipboardImage(receipt.clone())), WORKSPACE)
                .unwrap();
            assert_eq!(payload.is_some(), index == 1);
            previous_receipt = Some(receipt);
        }
        assert!(upload.images.is_empty());
        assert!(upload.paths.is_empty());
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(previous_receipt.unwrap())),
                    WORKSPACE,
                )
                .is_err()
        );
    }

    #[test]
    fn file_list_count_and_individual_size_are_bounded_before_reading() {
        assert!(matches!(
            read_with(vec![PathBuf::from("missing"); 17], || panic!("text flavor")),
            Err(WorkspaceIoErrorCode::ClipboardTooLarge)
        ));
        let files = Files::new();
        let path = files.write("oversized.png", b"\x89PNG\r\n\x1a\n");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(IMAGE_BYTES_MAX as u64 + 1)
            .unwrap();
        assert!(matches!(
            read_with(vec![path], || panic!("text flavor")),
            Err(WorkspaceIoErrorCode::ClipboardTooLarge)
        ));
    }

    #[test]
    fn multiple_images_share_one_total_byte_budget() {
        let files = Files::new();
        let paths = ["first.png", "second.png"]
            .map(|name| {
                let path = files.write(name, b"\x89PNG\r\n\x1a\n");
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len((IMAGE_BYTES_MAX / 2 + 1) as u64)
                    .unwrap();
                path
            })
            .into_iter()
            .collect();
        assert!(matches!(
            read_with(paths, || panic!("text flavor")),
            Err(WorkspaceIoErrorCode::ClipboardTooLarge)
        ));
    }

    #[test]
    fn fifo_without_a_writer_is_rejected() {
        use std::os::unix::ffi::OsStrExt;
        let files = Files::new();
        let path = files.0.join("image.png");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            read_with(vec![path], || panic!("text flavor")),
            Err(WorkspaceIoErrorCode::InvalidPath)
        ));
    }

    #[test]
    fn image_type_is_detected_from_bytes_instead_of_the_local_filename() {
        let files = Files::new();
        for (bytes, extension) in [
            (&b"\x89PNG\r\n\x1a\n"[..], "png"),
            (&b"\xff\xd8\xff"[..], "jpg"),
            (&b"GIF89a"[..], "gif"),
            (&b"RIFF0000WEBP"[..], "webp"),
        ] {
            let path = files.write("misleading.txt", bytes);
            let ReadResult::Images(upload) =
                read_with(vec![path], || panic!("text flavor")).unwrap()
            else {
                panic!("image upload")
            };
            let Request::StageClipboardImage(chunk) = upload.request() else {
                panic!("typed staging request")
            };
            assert_eq!(chunk.extension, extension);
            assert_eq!(chunk.data, bytes);
            assert!(uuid::Uuid::parse_str(&chunk.upload_id).is_ok());
            assert_eq!(chunk.sha256.len(), 64);
        }
        assert!(matches!(
            read_with(
                vec![files.write("unsupported.png", b"BMunsupported")],
                || { panic!("text flavor") }
            ),
            Err(WorkspaceIoErrorCode::InvalidPath)
        ));
    }

    #[test]
    fn text_uses_the_existing_payload_without_an_upload() {
        assert!(matches!(
            read_with(Vec::new(), || Some("한글 text".into())),
            Ok(ReadResult::Text(_))
        ));
    }
    #[test]
    fn image_chunks_never_return_a_mac_path_and_reject_early_or_wrong_acks() {
        let path = std::env::temp_dir().join(format!("deppy-image-{}.png", uuid::Uuid::new_v4()));
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(CHUNK_BYTES_MAX + 64, 0);
        std::fs::write(&path, &data).unwrap();
        let ReadResult::Images(mut upload) = read_with(vec![path.clone()], || {
            panic!("image must not read text flavor")
        })
        .unwrap() else {
            panic!("image upload")
        };
        let Request::StageClipboardImage(chunk) = upload.request() else {
            panic!("typed staging request")
        };
        assert_eq!(chunk.offset, 0);
        assert_eq!(chunk.data.len(), CHUNK_BYTES_MAX);
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(Receipt {
                        written: 1,
                        path: None
                    })),
                    WORKSPACE,
                )
                .is_err()
        );
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(Receipt {
                        written: CHUNK_BYTES_MAX as u64,
                        path: Some("/fake.png".into())
                    })),
                    WORKSPACE,
                )
                .is_err()
        );
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(Receipt {
                        written: CHUNK_BYTES_MAX as u64,
                        path: None
                    })),
                    WORKSPACE,
                )
                .unwrap()
                .is_none()
        );
        let Request::StageClipboardImage(chunk) = upload.request() else {
            panic!("next chunk")
        };
        assert_eq!(chunk.offset, CHUNK_BYTES_MAX as u64);
        assert_eq!(chunk.data.len(), 64);
        let remote_path = format!(
            "/home/ubuntu/.cache/deppy/ssh-clipboard/{}-{}.png",
            &WORKSPACE[..32],
            chunk.upload_id,
        );
        assert!(
            upload
                .accept(
                    Ok(Response::ClipboardImage(Receipt {
                        written: data.len() as u64,
                        path: Some(remote_path),
                    })),
                    WORKSPACE,
                )
                .unwrap()
                .is_some()
        );
        std::fs::remove_file(path).unwrap();
    }
}
