//! Private reattachment proofs, separate from nonsecret workspace settings.
use deppy_core::workspace_target::SshWorkspaceTarget;
use runtime::SshResume;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt},
        },
    },
    path::Path,
};

const RECORD_BYTES_MAX: u64 = 64 * 1024;
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    target: SshWorkspaceTarget,
    resume: SshResume,
}
fn name(target: &SshWorkspaceTarget) -> anyhow::Result<CString> {
    target.validate()?;
    let bytes = serde_json::to_vec(&(target.identity(), &target.remote_workspace_id))?;
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(CString::new(format!("{digest}.json"))?)
}
fn private_directory(data: &Path) -> anyhow::Result<File> {
    let path = data.join("ssh-resume");
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let path = CString::new(path.as_os_str().as_bytes())?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    anyhow::ensure!(fd >= 0, "ssh_resume_directory_unavailable");
    let dir = unsafe { File::from_raw_fd(fd) };
    let metadata = dir.metadata()?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "ssh_resume_directory_not_private"
    );
    Ok(dir)
}
fn open_record(dir: &File, filename: &CString) -> anyhow::Result<Option<File>> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        anyhow::bail!("ssh_resume_record_unavailable");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= RECORD_BYTES_MAX,
        "ssh_resume_record_not_private"
    );
    Ok(Some(file))
}
pub(crate) fn load(data: &Path, target: &SshWorkspaceTarget) -> anyhow::Result<Option<SshResume>> {
    let dir = private_directory(data)?;
    let Some(file) = open_record(&dir, &name(target)?)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(RECORD_BYTES_MAX + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= RECORD_BYTES_MAX as usize,
        "ssh_resume_record_too_large"
    );
    let record: Record = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        record.target.identity() == target.identity()
            && record.target.remote_workspace_id == target.remote_workspace_id,
        "ssh_resume_target_mismatch"
    );
    validate_resume(&record.resume)?;
    Ok(Some(record.resume))
}
fn validate_resume(resume: &SshResume) -> anyhow::Result<()> {
    anyhow::ensure!(
        [&resume.runtime_instance, &resume.lease]
            .iter()
            .all(|value| !value.is_empty()
                && value.len() <= 128
                && !value.chars().any(char::is_control)),
        "ssh_resume_invalid"
    );
    Ok(())
}
pub(crate) fn save(
    data: &Path,
    target: &SshWorkspaceTarget,
    resume: &SshResume,
) -> anyhow::Result<()> {
    validate_resume(resume)?;
    let filename = name(target)?;
    let bytes = serde_json::to_vec(&Record {
        target: target.clone(),
        resume: resume.clone(),
    })?;
    anyhow::ensure!(
        bytes.len() <= RECORD_BYTES_MAX as usize,
        "ssh_resume_record_too_large"
    );
    let dir = private_directory(data)?;
    // Existing entries must be owned regular private files; never follow a substituted symlink.
    let _ = open_record(&dir, &filename)?;
    let temporary = CString::new(format!(".{}.tmp", uuid::Uuid::new_v4()))?;
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    anyhow::ensure!(fd >= 0, "ssh_resume_write_unavailable");
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> anyhow::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        anyhow::ensure!(
            unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temporary.as_ptr(),
                    dir.as_raw_fd(),
                    filename.as_ptr(),
                )
            } == 0,
            "ssh_resume_replace_failed"
        );
        dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}
pub(crate) fn clear(data: &Path, target: &SshWorkspaceTarget) -> anyhow::Result<()> {
    let dir = private_directory(data)?;
    let filename = name(target)?;
    if open_record(&dir, &filename)?.is_some() {
        anyhow::ensure!(
            unsafe { libc::unlinkat(dir.as_raw_fd(), filename.as_ptr(), 0) } == 0,
            "ssh_resume_remove_failed"
        );
        dir.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_core::workspace_target::RemotePath;
    use std::os::unix::fs::PermissionsExt;
    fn target() -> SshWorkspaceTarget {
        SshWorkspaceTarget::new(
            "profile".into(),
            "host.test".into(),
            22,
            "ubuntu".into(),
            format!("SHA256:{}", "a".repeat(43)),
            "workspace".into(),
            RemotePath::new("/home/ubuntu/project").unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn ssh_resume_store_keeps_private_proof_for_exact_target_across_reopen() {
        let data = std::env::temp_dir().join(format!("deppy-ssh-resume-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&data).unwrap();
        let target = target();
        let resume = SshResume {
            runtime_instance: "runtime-a".into(),
            lease: "private-lease".into(),
        };
        save(&data, &target, &resume).unwrap();
        let loaded = load(&data, &target)
            .unwrap()
            .expect("restart must retain the lease instead of recreating sessions");
        assert_eq!(loaded.runtime_instance, resume.runtime_instance);
        assert_eq!(loaded.lease, resume.lease);
        let mut other = target.clone();
        other.account = "other".into();
        assert!(load(&data, &other).unwrap().is_none());
        let dir = data.join("ssh-resume");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in std::fs::read_dir(&dir).unwrap() {
            assert_eq!(
                file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn ssh_resume_store_refuses_symlink_directory_and_never_overwrites_destination() {
        let data =
            std::env::temp_dir().join(format!("deppy-ssh-resume-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&data).unwrap();
        let outside = data.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, data.join("ssh-resume")).unwrap();
        let resume = SshResume {
            runtime_instance: "runtime".into(),
            lease: "secret".into(),
        };
        assert!(
            save(&data, &target(), &resume).is_err(),
            "private proofs must not follow a substituted directory"
        );
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
        std::fs::remove_dir_all(data).unwrap();
    }
}
