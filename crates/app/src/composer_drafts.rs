//! Bounded session draft checkpoints. UI owns live text; immutable checkpoints share Arc bodies.
use std::collections::HashSet;
use std::hash::Hasher;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
pub(crate) const DRAFT_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const DRAFT_TOTAL_MAX_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const DRAFT_MAX_ITEMS: usize = 256;
pub(crate) const DRAFT_KEY_MAX_BYTES: usize = 8192;
pub(crate) const DRAFT_WORKSPACE_MAX_BYTES: usize = 4096;
pub(crate) const DRAFT_METADATA_MAX_BYTES: usize = 256 * 1024;
pub(crate) const DRAFT_FILE_MAX_BYTES: usize = 64 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DraftRecord {
    pub key: String,
    pub workspace_id: String,
    #[serde(default)]
    pub delivery_uncertain: bool,
    #[serde(with = "arc_text")]
    pub text: Arc<str>,
}
mod arc_text {
    pub fn serialize<S: serde::Serializer>(
        text: &std::sync::Arc<str>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.serialize_str(text)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<std::sync::Arc<str>, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(d)?;
        if text.len() > super::DRAFT_MAX_BYTES {
            return Err(serde::de::Error::custom("draft text limit"));
        }
        Ok(std::sync::Arc::from(text))
    }
}
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftSnapshot {
    #[serde(deserialize_with = "bounded_records")]
    pub drafts: Vec<DraftRecord>,
}
fn bounded_records<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<DraftRecord>, D::Error> {
    struct Records;
    impl<'de> serde::de::Visitor<'de> for Records {
        type Value = Vec<DraftRecord>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded session drafts")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut records = Vec::new();
            let mut bytes = 0usize;
            let mut metadata = 0usize;
            while let Some(record) = seq.next_element::<DraftRecord>()? {
                bytes = bytes.saturating_add(record.text.len());
                metadata = metadata.saturating_add(record.key.len() + record.workspace_id.len());
                if records.len() >= DRAFT_MAX_ITEMS
                    || bytes > DRAFT_TOTAL_MAX_BYTES
                    || metadata > DRAFT_METADATA_MAX_BYTES
                    || record.key.len() > DRAFT_KEY_MAX_BYTES
                    || record.workspace_id.len() > DRAFT_WORKSPACE_MAX_BYTES
                {
                    return Err(serde::de::Error::custom("draft store limit"));
                }
                records.push(record);
            }
            Ok(records)
        }
    }
    deserializer.deserialize_seq(Records)
}
pub(crate) struct DraftStartup {
    pub snapshot: DraftSnapshot,
    pub file_version: DraftFileVersion,
    pub error: Option<DraftError>,
}
impl DraftSnapshot {
    pub(crate) fn validate(&self) -> Result<(), DraftError> {
        let mut keys = HashSet::new();
        let mut bytes = 0usize;
        let mut metadata = 0usize;
        if self.drafts.len() > DRAFT_MAX_ITEMS {
            return Err(DraftError::LimitExceeded);
        }
        for draft in &self.drafts {
            bytes = bytes.saturating_add(draft.text.len());
            metadata = metadata.saturating_add(draft.key.len() + draft.workspace_id.len());
            if draft.key.is_empty()
                || draft.key.len() > DRAFT_KEY_MAX_BYTES
                || draft.workspace_id.is_empty()
                || draft.workspace_id.len() > DRAFT_WORKSPACE_MAX_BYTES
                || draft.text.len() > DRAFT_MAX_BYTES
                || !keys.insert(draft.key.as_str())
            {
                return Err(DraftError::LimitExceeded);
            }
        }
        if bytes > DRAFT_TOTAL_MAX_BYTES || metadata > DRAFT_METADATA_MAX_BYTES {
            return Err(DraftError::LimitExceeded);
        }
        Ok(())
    }
    pub(crate) fn load_startup(path: &Path) -> DraftStartup {
        let load = (|| {
            let Some(file) = open_draft_file(path)? else {
                return Ok((Self::default(), DraftFileVersion::Missing));
            };
            let mut reader = LimitedReader {
                file: file.take((DRAFT_FILE_MAX_BYTES + 1) as u64),
                read: 0,
                exceeded: false,
                hash: std::collections::hash_map::DefaultHasher::new(),
            };
            let result =
                serde_json::from_reader(std::io::BufReader::with_capacity(64 * 1024, &mut reader));
            let snapshot: Self = result.map_err(|error| {
                if reader.exceeded
                    || (error.is_data()
                        && (error.to_string().starts_with("draft text limit")
                            || error.to_string().starts_with("draft store limit")))
                {
                    DraftError::LimitExceeded
                } else if error.is_io() {
                    DraftError::ReadFailed
                } else {
                    DraftError::Corrupt
                }
            })?;
            snapshot.validate()?;
            Ok((
                snapshot,
                DraftFileVersion::Present {
                    bytes: reader.read,
                    fingerprint: reader.hash.finish(),
                },
            ))
        })();
        match load {
            Ok((snapshot, file_version)) => DraftStartup {
                snapshot,
                file_version,
                error: None,
            },
            Err(error) => DraftStartup {
                snapshot: Self::default(),
                file_version: DraftFileVersion::Missing,
                error: Some(error),
            },
        }
    }
    pub(crate) fn save_checked(
        &self,
        path: &Path,
        expected: DraftFileVersion,
    ) -> Result<DraftFileVersion, DraftError> {
        self.save_inner(path, Some(expected))
    }

    fn save_inner(
        &self,
        path: &Path,
        expected: Option<DraftFileVersion>,
    ) -> Result<DraftFileVersion, DraftError> {
        self.validate()?;
        if let Some(expected) = expected {
            check_file_version(path, expected)?;
        }
        let tmp = path.with_file_name(format!(".prompt-draft-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&tmp).map_err(|_| DraftError::WriteFailed)?;
        let result = (|| {
            let mut writer = LimitedWriter {
                file: std::io::BufWriter::with_capacity(64 * 1024, file),
                written: 0,
                exceeded: false,
                hash: std::collections::hash_map::DefaultHasher::new(),
            };
            if serde_json::to_writer_pretty(&mut writer, self).is_err() {
                return Err(if writer.exceeded {
                    DraftError::LimitExceeded
                } else {
                    DraftError::WriteFailed
                });
            }
            writer.file.flush().map_err(|_| DraftError::WriteFailed)?;
            writer
                .file
                .get_ref()
                .sync_all()
                .map_err(|_| DraftError::WriteFailed)?;
            let version = DraftFileVersion::Present {
                bytes: writer.written,
                fingerprint: writer.hash.finish(),
            };
            drop(writer);
            if let Some(expected) = expected {
                check_file_version(path, expected)?;
            }
            commit_temp(&tmp, path, expected)?;
            Ok(version)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftError {
    ReadFailed,
    Corrupt,
    LimitExceeded,
    WriteFailed,
    RecoveryRequired,
    WorkerUnavailable,
    StaleRevision,
    Conflict,
}

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DraftError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftFileVersion {
    Missing,
    Present { bytes: usize, fingerprint: u64 },
}

fn commit_temp(
    tmp: &Path,
    path: &Path,
    expected: Option<DraftFileVersion>,
) -> Result<(), DraftError> {
    if expected == Some(DraftFileVersion::Missing) {
        // Atomic no-clobber seed: a real draft may arrive between the final check and commit.
        std::fs::hard_link(tmp, path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                DraftError::Conflict
            } else {
                DraftError::WriteFailed
            }
        })?;
        let _ = std::fs::remove_file(tmp);
    } else {
        std::fs::rename(tmp, path).map_err(|_| DraftError::WriteFailed)?;
    }
    Ok(())
}

fn open_draft_file(path: &Path) -> Result<Option<std::fs::File>, DraftError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(DraftError::ReadFailed),
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(DraftError::ReadFailed);
    }
    Ok(Some(file))
}

struct LimitedReader {
    file: std::io::Take<std::fs::File>,
    read: usize,
    exceeded: bool,
    hash: std::collections::hash_map::DefaultHasher,
}
impl Read for LimitedReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(bytes)?;
        self.read += count;
        if self.read > DRAFT_FILE_MAX_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::other("draft file limit"));
        }
        self.hash.write(&bytes[..count]);
        Ok(count)
    }
}

/// Verification shares the bounded regular-file opener but never allocates the entire file.
/// The hasher is incremental, identical to startup and the streaming writer.
fn read_file_version(path: &Path) -> Result<DraftFileVersion, DraftError> {
    let Some(file) = open_draft_file(path)? else {
        return Ok(DraftFileVersion::Missing);
    };
    let mut reader = file.take((DRAFT_FILE_MAX_BYTES + 1) as u64);
    let mut scratch = [0u8; 64 * 1024];
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut bytes = 0usize;
    loop {
        let count = reader
            .read(&mut scratch)
            .map_err(|_| DraftError::ReadFailed)?;
        if count == 0 {
            break;
        }
        bytes += count;
        if bytes > DRAFT_FILE_MAX_BYTES {
            return Err(DraftError::LimitExceeded);
        }
        hash.write(&scratch[..count]);
    }
    Ok(DraftFileVersion::Present {
        bytes,
        fingerprint: hash.finish(),
    })
}

fn check_file_version(path: &Path, expected: DraftFileVersion) -> Result<(), DraftError> {
    let current = read_file_version(path)?;
    if current == expected {
        Ok(())
    } else {
        Err(DraftError::Conflict)
    }
}

struct LimitedWriter {
    file: std::io::BufWriter<std::fs::File>,
    written: usize,
    exceeded: bool,
    hash: std::collections::hash_map::DefaultHasher,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > DRAFT_FILE_MAX_BYTES.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(std::io::Error::other("prompt draft size limit"));
        }
        let written = self.file.write(bytes)?;
        self.written += written;
        self.hash.write(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("deppy-pr5-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }
    fn snapshot(text: &str) -> DraftSnapshot {
        DraftSnapshot {
            drafts: vec![DraftRecord {
                key: "session-A".into(),
                workspace_id: "ws".into(),
                delivery_uncertain: false,
                text: Arc::from(text),
            }],
        }
    }
    #[test]
    fn pr5_temp_store_roundtrip_empty_unicode_and_external_conflict_preserves_original() {
        let dir = directory();
        let path = dir.join("drafts.json");
        let missing = DraftSnapshot::load_startup(&path);
        assert!(missing.error.is_none());
        assert_eq!(missing.file_version, DraftFileVersion::Missing);
        let first = snapshot("한글 👩🏽‍💻\nsecond line");
        let version = first.save_checked(&path, missing.file_version).unwrap();
        let loaded = DraftSnapshot::load_startup(&path);
        assert!(loaded.error.is_none());
        assert_eq!(loaded.snapshot, first);
        assert_eq!(
            loaded.file_version, version,
            "reader/writer fingerprint matches"
        );
        let empty = DraftSnapshot::default();
        let version = empty.save_checked(&path, version).unwrap();
        assert_eq!(DraftSnapshot::load_startup(&path).snapshot, empty);
        std::fs::write(&path, b"{\"drafts\":[] }\n").unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            snapshot("unsaved").save_checked(&path, version),
            Err(DraftError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr5_corrupt_read_failed_and_over_budget_keep_exact_disk_bytes() {
        let dir = directory();
        let path = dir.join("drafts.json");
        std::fs::write(&path, b"{ corrupt original").unwrap();
        assert_eq!(
            DraftSnapshot::load_startup(&path).error,
            Some(DraftError::Corrupt)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{ corrupt original");
        let over = snapshot(&"x".repeat(DRAFT_MAX_BYTES + 1));
        assert_eq!(over.validate(), Err(DraftError::LimitExceeded));
        std::fs::write(&path, serde_json::to_vec(&over).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(DraftSnapshot::load_startup(&path).error.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            DraftSnapshot::load_startup(&path).error,
            Some(DraftError::ReadFailed)
        );
        assert!(path.is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn pr5_same_handle_loader_rejects_symlink_and_fifo_without_touching_target() {
        use std::os::unix::fs::symlink;
        let dir = directory();
        let path = dir.join("drafts.json");
        let target = dir.join("real.json");
        std::fs::write(&target, b"{\"drafts\":[]}").unwrap();
        symlink(&target, &path).unwrap();
        assert_eq!(
            DraftSnapshot::load_startup(&path).error,
            Some(DraftError::ReadFailed)
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"drafts\":[]}");
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(
            DraftSnapshot::load_startup(&path).error,
            Some(DraftError::ReadFailed)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn pr5_total_item_metadata_and_serialized_file_limits_fail_before_original_commit() {
        let dir = directory();
        let path = dir.join("drafts.json");
        let first = snapshot("original");
        let version = first
            .save_checked(&path, DraftFileVersion::Missing)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut too_many = DraftSnapshot::default();
        for i in 0..=DRAFT_MAX_ITEMS {
            too_many.drafts.push(DraftRecord {
                key: format!("session-{i}"),
                workspace_id: "ws".into(),
                delivery_uncertain: false,
                text: Arc::from("body"),
            })
        }
        assert_eq!(too_many.validate(), Err(DraftError::LimitExceeded));
        let mut too_large = DraftSnapshot::default();
        for i in 0..=32 {
            too_large.drafts.push(DraftRecord {
                key: format!("session-{i}"),
                workspace_id: "ws".into(),
                delivery_uncertain: false,
                text: Arc::from("x".repeat(DRAFT_MAX_BYTES)),
            })
        }
        assert_eq!(too_large.validate(), Err(DraftError::LimitExceeded));
        let escaped: Arc<str> = Arc::from("\0".repeat(DRAFT_MAX_BYTES));
        let serialized = DraftSnapshot {
            drafts: (0..12)
                .map(|i| DraftRecord {
                    key: format!("session-{i}"),
                    workspace_id: "ws".into(),
                    delivery_uncertain: false,
                    text: Arc::clone(&escaped),
                })
                .collect(),
        };
        assert!(serialized.validate().is_ok());
        assert_eq!(
            serialized.save_checked(&path, version),
            Err(DraftError::LimitExceeded)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temporary stream removed on failure"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
