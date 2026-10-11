use super::contracts::*;
use deppy_core::project_keys::KeySource;
use secret::SecretString;
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

/// A verified directory remains open until all reads, writes, and child spawning finish.
pub(crate) struct AnchoredRoot {
    file: File,
    path: PathBuf,
}
impl AnchoredRoot {
    pub(crate) fn open(path: &Path) -> Result<Self, ProjectKeysError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
                .open(path)
                .map_err(source_error)?;
            crate::folder_identity::from_handle(&file).ok_or(ProjectKeysError::SourceUnsafe)?;
            Ok(Self {
                file,
                path: path.into(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(ProjectKeysError::SourceUnsafe)
        }
    }
    pub(crate) fn identity(&self) -> crate::folder_identity::FolderIdentity {
        // Construction and directory-only openat guarantee that this descriptor is a directory.
        crate::folder_identity::from_handle(&self.file).expect("anchored directory identity")
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn file(&self) -> &File {
        &self.file
    }
    pub(crate) fn open_directory(&self, relative: &Path) -> Result<Self, ProjectKeysError> {
        let relative = if relative.is_absolute() {
            relative
                .strip_prefix(&self.path)
                .map_err(|_| ProjectKeysError::InvalidInput)?
        } else {
            relative
        };
        let mut file = self
            .file
            .try_clone()
            .map_err(|_| ProjectKeysError::SourceUnsafe)?;
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => {
                    file = open_at(&file, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                }
                _ => return Err(ProjectKeysError::SourceUnsafe),
            }
        }
        Ok(Self {
            file,
            path: self.path.join(relative),
        })
    }
}
fn source_error(error: std::io::Error) -> ProjectKeysError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProjectKeysError::SourceMissing
    } else {
        ProjectKeysError::SourceUnsafe
    }
}
#[cfg(unix)]
fn name_cstring(name: &OsStr) -> Result<CString, ProjectKeysError> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(name.as_bytes()).map_err(|_| ProjectKeysError::InvalidInput)
}
#[cfg(unix)]
fn open_at(
    parent: &File,
    name: &OsStr,
    flags: i32,
    mode: libc::mode_t,
) -> Result<File, ProjectKeysError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = name_cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(source_error(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
#[cfg(not(unix))]
fn open_at(_: &File, _: &OsStr, _: i32, _: libc::mode_t) -> Result<File, ProjectKeysError> {
    Err(ProjectKeysError::SourceUnsafe)
}
struct SourceLocation {
    parent: AnchoredRoot,
    name: OsString,
}
fn source_location(
    root: &AnchoredRoot,
    source: &KeySource,
) -> Result<SourceLocation, ProjectKeysError> {
    source
        .validate()
        .map_err(|_| ProjectKeysError::InvalidInput)?;
    let selected = match source {
        KeySource::Dotenv { file } => Path::new(file),
        KeySource::PemFile { path } => Path::new(path),
        _ => return Err(ProjectKeysError::InvalidInput),
    };
    let absolute;
    let (base, relative) = if selected.is_absolute() {
        if let Ok(relative) = selected.strip_prefix(root.path()) {
            (root, relative)
        } else {
            absolute = AnchoredRoot::open(Path::new("/"))?;
            (
                &absolute,
                selected
                    .strip_prefix("/")
                    .map_err(|_| ProjectKeysError::InvalidInput)?,
            )
        }
    } else {
        (root, selected)
    };
    let name = relative
        .file_name()
        .ok_or(ProjectKeysError::InvalidInput)?
        .to_owned();
    let parent = base.open_directory(relative.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(SourceLocation { parent, name })
}
fn read_location(
    location: &SourceLocation,
    max: usize,
) -> Result<(SecretString, FileIdentity), ProjectKeysError> {
    let file = open_at(&location.parent.file, &location.name, libc::O_RDONLY, 0)?;
    let metadata = file
        .metadata()
        .map_err(|_| ProjectKeysError::SourceUnsafe)?;
    if !metadata.is_file() {
        return Err(ProjectKeysError::SourceUnsafe);
    }
    let identity = FileIdentity::of(&metadata);
    let mut bytes = Vec::new();
    if file.take((max + 1) as u64).read_to_end(&mut bytes).is_err()
        || bytes.len() > max
        || bytes.contains(&0)
    {
        bytes.fill(0);
        return Err(ProjectKeysError::SourceUnsafe);
    }
    match String::from_utf8(bytes) {
        Ok(value) => Ok((SecretString::new(value), identity)),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.fill(0);
            Err(ProjectKeysError::SourceUnsafe)
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}
impl FileIdentity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Self { dev: 0, ino: 0 }
        }
    }
}
fn content_limit(source: &KeySource) -> usize {
    if matches!(source, KeySource::Dotenv { .. }) {
        runtime::dotenv::DOTENV_TOTAL_BYTES_MAX
    } else {
        SECRET_INPUT_BYTES_MAX
    }
}
pub(crate) fn read_source_anchored(
    root: &AnchoredRoot,
    source: &KeySource,
) -> Result<BTreeMap<String, SecretString>, ProjectKeysError> {
    let location = source_location(root, source)?;
    let (content, _) = read_location(&location, content_limit(source))?;
    match source {
        KeySource::Dotenv { .. } => {
            let values = runtime::dotenv::parse_dotenv_bounded(
                content.expose(),
                runtime::dotenv::DOTENV_ENTRIES_MAX,
            )
            .map_err(|_| ProjectKeysError::SourceUnsafe)?;
            let mut result = BTreeMap::new();
            for (name, value) in values {
                let value = SecretString::new(value);
                if deppy_core::credential_env::valid_name(&name) {
                    result.insert(name, value);
                }
            }
            if result.len() > deppy_core::project_keys::PROJECT_KEYS_MAX {
                return Err(ProjectKeysError::InvalidInput);
            }
            Ok(result)
        }
        KeySource::PemFile { .. } => Ok(BTreeMap::from([("PRIVATE_KEY".into(), content)])),
        _ => Err(ProjectKeysError::InvalidInput),
    }
}
#[cfg(test)]
pub(crate) fn read_source(
    root: &Path,
    source: &KeySource,
) -> Result<BTreeMap<String, SecretString>, ProjectKeysError> {
    read_source_anchored(&AnchoredRoot::open(root)?, source)
}
fn replacement(
    source: &KeySource,
    name: &str,
    value: &SecretString,
    expected: Option<&SecretString>,
    original: &SecretString,
) -> Result<SecretString, ProjectKeysError> {
    if !deppy_core::credential_env::valid_name(name) || value.expose().contains('\0') {
        return Err(ProjectKeysError::InvalidInput);
    }
    let next = match source {
        KeySource::PemFile { .. } => {
            if expected.is_none_or(|expected| original.expose() != expected.expose()) {
                return Err(ProjectKeysError::SourceChanged);
            }
            if value.expose().len() > SECRET_INPUT_BYTES_MAX {
                return Err(ProjectKeysError::InvalidInput);
            }
            SecretString::new(value.expose().into())
        }
        KeySource::Dotenv { .. } => {
            let parsed = runtime::dotenv::parse_dotenv_bounded(
                original.expose(),
                runtime::dotenv::DOTENV_ENTRIES_MAX,
            )
            .map_err(|_| ProjectKeysError::SourceUnsafe)?;
            let mut values = BTreeMap::new();
            for (key, val) in parsed {
                values.insert(key, SecretString::new(val));
            }
            if values.get(name).map(|v| v.expose()) != expected.map(|v| v.expose()) {
                return Err(ProjectKeysError::SourceChanged);
            }
            if value.expose().len() > runtime::dotenv::DOTENV_VALUE_BYTES_MAX
                || value.expose().contains(['\n', '\r'])
            {
                return Err(ProjectKeysError::InvalidInput);
            }
            let quote = if !value.expose().contains('"') {
                '"'
            } else if !value.expose().contains('\'') {
                '\''
            } else {
                return Err(ProjectKeysError::InvalidInput);
            };
            let mut next = String::new();
            for line in original.expose().lines() {
                let trimmed = line.trim().strip_prefix("export ").unwrap_or(line.trim());
                if trimmed
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == name)
                {
                    continue;
                }
                next.push_str(line);
                next.push('\n');
            }
            next.push_str(name);
            next.push('=');
            next.push(quote);
            next.push_str(value.expose());
            next.push(quote);
            next.push('\n');
            SecretString::new(next)
        }
        _ => return Err(ProjectKeysError::InvalidInput),
    };
    if next.expose().len() > content_limit(source) {
        return Err(ProjectKeysError::InvalidInput);
    }
    if matches!(source, KeySource::Dotenv { .. }) {
        let parsed = runtime::dotenv::parse_dotenv_bounded(
            next.expose(),
            runtime::dotenv::DOTENV_ENTRIES_MAX,
        )
        .map_err(|_| ProjectKeysError::InvalidInput)?;
        let parsed: Vec<_> = parsed
            .into_iter()
            .map(|(name, value)| (name, SecretString::new(value)))
            .collect();
        if parsed
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .is_none_or(|(_, parsed)| parsed.expose() != value.expose())
        {
            return Err(ProjectKeysError::InvalidInput);
        }
        let unique = parsed
            .iter()
            .filter(|(key, _)| deppy_core::credential_env::valid_name(key))
            .map(|(key, _)| key)
            .collect::<std::collections::BTreeSet<_>>();
        if unique.len() > deppy_core::project_keys::PROJECT_KEYS_MAX {
            return Err(ProjectKeysError::InvalidInput);
        }
    }
    Ok(next)
}
/// The protected journal is persisted by the service before apply. This object retains no plaintext Debug/Clone API.
pub(crate) struct PreparedSourceWrite {
    location: SourceLocation,
    source: KeySource,
    original: SecretString,
    replacement: SecretString,
    original_identity: FileIdentity,
    applied: Option<FileIdentity>,
    recovery: SourceRecoveryIdentity,
}
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct SourceRecoveryIdentity {
    version: u8,
    root_dev: i64,
    root_ino: i64,
    root_volume: Option<[u8; 16]>,
    parent_dev: i64,
    parent_ino: i64,
    parent_volume: Option<[u8; 16]>,
}
impl SourceRecoveryIdentity {
    fn capture(root: &AnchoredRoot, parent: &AnchoredRoot) -> Self {
        let root = root.identity();
        let parent = parent.identity();
        Self {
            version: 1,
            root_dev: root.anchor.dev,
            root_ino: root.anchor.ino,
            root_volume: root.volume.map(|volume| *volume.as_bytes()),
            parent_dev: parent.anchor.dev,
            parent_ino: parent.anchor.ino,
            parent_volume: parent.volume.map(|volume| *volume.as_bytes()),
        }
    }
}
impl PreparedSourceWrite {
    pub(crate) fn original(&self) -> &SecretString {
        &self.original
    }
    pub(crate) fn replacement(&self) -> &SecretString {
        &self.replacement
    }
    pub(crate) fn recovery_identity(&self) -> SecretString {
        SecretString::new(
            serde_json::to_string(&self.recovery).expect("scalar source recovery identity"),
        )
    }
    pub(crate) fn apply(&mut self) -> Result<(), ProjectKeysError> {
        if self.applied.is_some() {
            return Err(ProjectKeysError::InvalidInput);
        }
        self.applied = Some(replace_location(
            &self.location,
            &self.original,
            &self.replacement,
            Some(self.original_identity),
            content_limit(&self.source),
        )?);
        self.location
            .parent
            .file
            .sync_all()
            .map_err(|_| ProjectKeysError::SourceUnsafe)
    }
    pub(crate) fn rollback_if_unchanged(&mut self) -> Result<(), ProjectKeysError> {
        if let Some(identity) = self.applied {
            replace_location(
                &self.location,
                &self.replacement,
                &self.original,
                Some(identity),
                content_limit(&self.source),
            )?;
            self.applied = None;
            self.location
                .parent
                .file
                .sync_all()
                .map_err(|_| ProjectKeysError::SourceUnsafe)?;
        }
        Ok(())
    }
    pub(crate) fn finish(self) {}
}
pub(crate) fn prepare_source_write(
    root: &AnchoredRoot,
    source: &KeySource,
    name: &str,
    value: &SecretString,
    expected: Option<&SecretString>,
) -> Result<PreparedSourceWrite, ProjectKeysError> {
    let location = source_location(root, source)?;
    let (original, original_identity) = read_location(&location, content_limit(source))?;
    let replacement = replacement(source, name, value, expected, &original)?;
    let recovery = SourceRecoveryIdentity::capture(root, &location.parent);
    Ok(PreparedSourceWrite {
        location,
        source: source.clone(),
        original,
        replacement,
        original_identity,
        applied: None,
        recovery,
    })
}
#[cfg(unix)]
fn replace_location(
    location: &SourceLocation,
    expected: &SecretString,
    next: &SecretString,
    identity: Option<FileIdentity>,
    max: usize,
) -> Result<FileIdentity, ProjectKeysError> {
    use std::os::fd::AsRawFd;
    let temp = OsString::from(format!(".deppy-key-{}.tmp", uuid::Uuid::new_v4()));
    let temp_name = name_cstring(&temp)?;
    let name = name_cstring(&location.name)?;
    let result = (|| {
        let mut file = open_at(
            &location.parent.file,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        file.write_all(next.expose().as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| ProjectKeysError::SourceUnsafe)?;
        let new_identity = FileIdentity::of(
            &file
                .metadata()
                .map_err(|_| ProjectKeysError::SourceUnsafe)?,
        );
        let (current, current_identity) = read_location(location, max)?;
        if current.expose() != expected.expose()
            || identity.is_some_and(|identity| identity != current_identity)
        {
            return Err(ProjectKeysError::SourceChanged);
        }
        if unsafe {
            libc::renameat(
                location.parent.file.as_raw_fd(),
                temp_name.as_ptr(),
                location.parent.file.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(ProjectKeysError::SourceUnsafe);
        }
        // The rename has happened: expose its identity so a failed later DB publication can roll it back.
        Ok(new_identity)
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(location.parent.file.as_raw_fd(), temp_name.as_ptr(), 0);
        }
    }
    result
}
#[cfg(not(unix))]
fn replace_location(
    _: &SourceLocation,
    _: &SecretString,
    _: &SecretString,
    _: Option<FileIdentity>,
    _: usize,
) -> Result<FileIdentity, ProjectKeysError> {
    Err(ProjectKeysError::SourceUnsafe)
}
pub(crate) fn recover_source_write_with_identity(
    root: &AnchoredRoot,
    source: &KeySource,
    original: &SecretString,
    replacement: &SecretString,
    identity: &SecretString,
) -> Result<(), ProjectKeysError> {
    recover_source_write_inner(root, source, original, replacement, identity)
}
fn recover_source_write_inner(
    root: &AnchoredRoot,
    source: &KeySource,
    original: &SecretString,
    replacement: &SecretString,
    identity: &SecretString,
) -> Result<(), ProjectKeysError> {
    if original.expose().len() > content_limit(source)
        || replacement.expose().len() > content_limit(source)
    {
        return Err(ProjectKeysError::SourceUnsafe);
    }
    let location = source_location(root, source)?;
    {
        if identity.expose().len() > 1024 {
            return Err(ProjectKeysError::SourceUnsafe);
        }
        let expected: SourceRecoveryIdentity =
            serde_json::from_str(identity.expose()).map_err(|_| ProjectKeysError::SourceUnsafe)?;
        if expected != SourceRecoveryIdentity::capture(root, &location.parent) {
            return Err(ProjectKeysError::SourceChanged);
        }
    }
    let (current, identity) = read_location(&location, content_limit(source))?;
    if current.expose() == original.expose() {
        return Ok(());
    }
    replace_location(
        &location,
        replacement,
        original,
        Some(identity),
        content_limit(source),
    )?;
    location
        .parent
        .file
        .sync_all()
        .map_err(|_| ProjectKeysError::SourceUnsafe)
}
#[cfg(test)]
pub(crate) fn write_source(
    root: &Path,
    source: &KeySource,
    name: &str,
    value: &SecretString,
    expected: Option<&SecretString>,
) -> Result<(), ProjectKeysError> {
    let mut write =
        prepare_source_write(&AnchoredRoot::open(root)?, source, name, value, expected)?;
    write.apply()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journal_recovery_rejects_replaced_parent_even_when_file_bytes_match() {
        let root = temp();
        let anchored = AnchoredRoot::open(&root).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/key.pem"), "fixture-old-secret").unwrap();
        let source = KeySource::PemFile {
            path: "nested/key.pem".into(),
        };
        let mut stage = prepare_source_write(
            &anchored,
            &source,
            "PRIVATE_KEY",
            &SecretString::new("fixture-replacement".into()),
            Some(&SecretString::new("fixture-old-secret".into())),
        )
        .unwrap();
        let identity = stage.recovery_identity();
        assert!(identity.expose().len() <= 1024);
        assert!(!identity.expose().contains("fixture-old-secret"));
        stage.apply().unwrap();
        std::fs::rename(root.join("nested"), root.join("old-parent")).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/key.pem"), "fixture-replacement").unwrap();
        assert!(matches!(
            recover_source_write_with_identity(
                &anchored,
                &source,
                stage.original(),
                stage.replacement(),
                &identity
            ),
            Err(ProjectKeysError::SourceChanged)
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("nested/key.pem")).unwrap(),
            "fixture-replacement"
        );
        stage.rollback_if_unchanged().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("old-parent/key.pem")).unwrap(),
            "fixture-old-secret"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn maximum_dotenv_value_round_trips_and_oversized_pem_write_is_atomic() {
        let root = temp();
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        std::fs::write(root.join(".env"), "TOKEN=old\n").unwrap();
        let maximum = SecretString::new("x".repeat(runtime::dotenv::DOTENV_VALUE_BYTES_MAX));
        write_source(
            &root,
            &source,
            "TOKEN",
            &maximum,
            Some(&SecretString::new("old".into())),
        )
        .unwrap();
        assert_eq!(
            read_source(&root, &source).unwrap()["TOKEN"].expose(),
            maximum.expose()
        );
        std::fs::write(root.join("key.pem"), "fixture-pem").unwrap();
        let pem = KeySource::PemFile {
            path: "key.pem".into(),
        };
        assert!(matches!(
            write_source(
                &root,
                &pem,
                "PRIVATE_KEY",
                &SecretString::new("x".repeat(SECRET_INPUT_BYTES_MAX + 1)),
                Some(&SecretString::new("fixture-pem".into()))
            ),
            Err(ProjectKeysError::InvalidInput)
        ));
        assert_eq!(
            std::fs::read_to_string(root.join("key.pem")).unwrap(),
            "fixture-pem"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn anchored_reads_and_writes_keep_original_workspace_after_path_replacement() {
        let root = temp();
        let old = root.with_extension("original");
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        std::fs::write(root.join(".env"), "TOKEN=original\n").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let mut stage = prepare_source_write(
            &anchored,
            &source,
            "TOKEN",
            &SecretString::new("changed".into()),
            Some(&SecretString::new("original".into())),
        )
        .unwrap();
        std::fs::rename(&root, &old).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".env"), "TOKEN=untrusted-replacement\n").unwrap();
        assert_eq!(
            read_source_anchored(&anchored, &source).unwrap()["TOKEN"].expose(),
            "original"
        );
        stage.apply().unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join(".env")).unwrap(),
            "TOKEN=\"changed\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".env")).unwrap(),
            "TOKEN=untrusted-replacement\n"
        );
        stage.rollback_if_unchanged().unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join(".env")).unwrap(),
            "TOKEN=original\n"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(old).unwrap();
    }
    #[test]
    fn staged_rollback_and_recovery_preserve_intervening_file_edits() {
        let root = temp();
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        std::fs::write(root.join(".env"), "TOKEN=old\n").unwrap();
        let anchored = AnchoredRoot::open(&root).unwrap();
        let mut stage = prepare_source_write(
            &anchored,
            &source,
            "TOKEN",
            &SecretString::new("next".into()),
            Some(&SecretString::new("old".into())),
        )
        .unwrap();
        stage.apply().unwrap();
        recover_source_write_with_identity(
            &anchored,
            &source,
            stage.original(),
            stage.replacement(),
            &stage.recovery_identity(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(".env")).unwrap(),
            "TOKEN=old\n"
        );
        // A different writer must never be overwritten by either rollback path.
        std::fs::write(root.join(".env"), "TOKEN=external\n").unwrap();
        assert!(matches!(
            stage.rollback_if_unchanged(),
            Err(ProjectKeysError::SourceChanged)
        ));
        assert!(matches!(
            recover_source_write_with_identity(
                &anchored,
                &source,
                stage.original(),
                stage.replacement(),
                &stage.recovery_identity()
            ),
            Err(ProjectKeysError::SourceChanged)
        ));
        assert_eq!(
            std::fs::read_to_string(root.join(".env")).unwrap(),
            "TOKEN=external\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_replacement_leaves_original_unchanged() {
        let root = temp();
        let path = root.join(".env");
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        let original = "TOKEN=old\n";
        std::fs::write(&path, original).unwrap();
        let result = write_source(
            &root,
            &source,
            "TOKEN",
            &SecretString::new("x".repeat(runtime::dotenv::DOTENV_VALUE_BYTES_MAX + 1)),
            Some(&SecretString::new("old".into())),
        );
        let unchanged = std::fs::read_to_string(&path).unwrap() == original;
        std::fs::remove_dir_all(root).unwrap();
        assert!(matches!(result, Err(ProjectKeysError::InvalidInput)));
        assert!(unchanged);
    }
    #[test]
    fn generated_file_limit_is_checked_before_replacement() {
        let root = temp();
        let path = root.join(".env");
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        let original = format!(
            "#{}\nTOKEN=old\n",
            "x".repeat(runtime::dotenv::DOTENV_TOTAL_BYTES_MAX - 20)
        );
        std::fs::write(&path, &original).unwrap();
        let result = write_source(
            &root,
            &source,
            "TOKEN",
            &SecretString::new("x".repeat(32)),
            Some(&SecretString::new("old".into())),
        );
        let unchanged = std::fs::read_to_string(&path).unwrap() == original;
        std::fs::remove_dir_all(root).unwrap();
        assert!(matches!(result, Err(ProjectKeysError::InvalidInput)));
        assert!(unchanged);
    }
    #[cfg(unix)]
    #[test]
    fn source_parent_symlink_is_rejected() {
        let root = temp();
        let outside = temp();
        std::fs::write(outside.join("key.pem"), "fixture-private-key").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        let result = read_source(
            &root,
            &KeySource::PemFile {
                path: "linked/key.pem".into(),
            },
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
        assert!(matches!(result, Err(ProjectKeysError::SourceUnsafe)));
    }
    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "deppy-project-keys-source-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn selected_source_reads_latest_original_and_keeps_empty_distinct_from_missing() {
        let root = temp();
        std::fs::write(root.join(".env"), "TOKEN=first\nEMPTY=\n").unwrap();
        let source = KeySource::Dotenv {
            file: ".env".into(),
        };
        let first = read_source(&root, &source).unwrap();
        assert_eq!(first["TOKEN"].expose(), "first");
        assert_eq!(first["EMPTY"].expose(), "");
        std::fs::write(root.join(".env"), "TOKEN=second\n").unwrap();
        assert_eq!(
            read_source(&root, &source).unwrap()["TOKEN"].expose(),
            "second"
        );
        assert!(matches!(
            read_source(
                &root,
                &KeySource::Dotenv {
                    file: ".env.local".into()
                }
            ),
            Err(ProjectKeysError::SourceMissing)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn selected_source_rejects_symlink_and_oversized_pem() {
        let root = temp();
        std::fs::write(root.join("target"), "TOKEN=secret\n").unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join(".env")).unwrap();
        assert!(matches!(
            read_source(
                &root,
                &KeySource::Dotenv {
                    file: ".env".into()
                }
            ),
            Err(ProjectKeysError::SourceUnsafe)
        ));
        std::fs::write(root.join("key.pem"), vec![b'x'; SECRET_INPUT_BYTES_MAX + 1]).unwrap();
        assert!(matches!(
            read_source(
                &root,
                &KeySource::PemFile {
                    path: root.join("key.pem").to_string_lossy().into()
                }
            ),
            Err(ProjectKeysError::SourceUnsafe)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
