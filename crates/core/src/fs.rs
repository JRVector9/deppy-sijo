//! 원자적 파일 기록 관례 (tmp + rename) — config/known_hosts/cert/아카이브 저장 공통.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// 고유한 임시 파일을 배타적으로 생성해 기록하고 같은 디렉터리에서 rename한다.
/// 기존 대상의 permissions를 보존하며, 기록/flush/rename 실패 시 자신의 temp만 지운다.
/// 부모 디렉터리 존재는 호출측이 보장한다.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path)?;
    // create_new rejects every existing entry, including symlinks. An unrelated writer or
    // pre-existing .deppytmp can never become the object written by this call.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let result = (|| {
        match std::fs::metadata(path) {
            Ok(metadata) => file.set_permissions(metadata.permissions())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn tmp_path(path: &Path) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("파일명이 없는 경로: {}", path.display()),
        ));
    };
    let mut tmp = name.to_os_string();
    tmp.push(format!(".deppytmp-{}", uuid::Uuid::new_v4()));
    Ok(path.with_file_name(tmp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deppy-core-fs-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn 기록_후_내용이_교체되고_tmp가_남지_않는다() {
        let dir = temp_dir("write");
        let path = dir.join("config.toml");
        std::fs::write(&path, b"old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!dir.join("config.toml.deppytmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 확장자가_없는_파일명도_처리한다() {
        let dir = temp_dir("noext");
        let path = dir.join("known_hosts");
        atomic_write(&path, b"host fp\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"host fp\n");
        assert!(!dir.join("known_hosts.deppytmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 파일명_없는_경로는_에러다() {
        assert!(atomic_write(Path::new("/"), b"x").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn full_fix_20261014_atomic_write_preserves_preexisting_temp_symlink_target() {
        use std::os::unix::fs::symlink;
        let dir =
            std::env::temp_dir().join(format!("deppy-atomic-symlink-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let intended = dir.join("config.toml");
        let victim = dir.join("other-user-file");
        let temporary = dir.join("config.toml.deppytmp");
        std::fs::write(&intended, b"old config").unwrap();
        std::fs::write(&victim, b"must preserve").unwrap();
        symlink(&victim, &temporary).unwrap();
        atomic_write(&intended, b"new config").unwrap();
        let victim_content = std::fs::read(&victim).unwrap();
        let published_is_symlink = std::fs::symlink_metadata(&intended)
            .unwrap()
            .file_type()
            .is_symlink();
        eprintln!(
            "production helper result: victim={:?}, published_config_is_symlink={published_is_symlink}",
            String::from_utf8_lossy(&victim_content)
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            victim_content, b"must preserve",
            "atomic save truncated a different existing user file through its fixed temp symlink"
        );
        assert!(
            !published_is_symlink,
            "save published the temp symlink as its config"
        );
    }
    #[test]
    fn full_fix_20261014_atomic_write_parallel_saves_have_distinct_temporary_objects() {
        use std::sync::{Arc, Barrier};
        let dir = std::env::temp_dir().join(format!(
            "deppy-atomic-concurrent-audit-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = Arc::new(dir.join("config.toml"));
        let barrier = Arc::new(Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|n| {
                let target = Arc::clone(&target);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let bytes = vec![b'A' + n; 1024 * 1024];
                    let mut errors = 0usize;
                    for _ in 0..30 {
                        barrier.wait();
                        if atomic_write(&target, &bytes).is_err() {
                            errors += 1;
                        }
                        barrier.wait();
                    }
                    errors
                })
            })
            .collect();
        let errors: usize = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum();
        let bytes = std::fs::read(&*target).unwrap();
        let valid = bytes.len() == 1024 * 1024 && bytes.iter().all(|byte| *byte == bytes[0]);
        eprintln!(
            "actual concurrent production saves: failed={errors}/120 final_payload_valid={valid}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            errors, 0,
            "unrelated successful writers share one temporary name and fail rename"
        );
        assert!(valid, "parallel saves mixed/truncated payloads");
    }

    #[cfg(unix)]
    #[test]
    fn full_fix_20261014_atomic_write_preserves_existing_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = dir.join("private-config");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn full_fix_20261014_atomic_write_cleans_only_its_temp_on_publication_failure() {
        let dir = temp_dir("rename-failure");
        let path = dir.join("config");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("existing"), b"retain").unwrap();
        assert!(atomic_write(&path, b"new").is_err());
        assert_eq!(std::fs::read(path.join("existing")).unwrap(), b"retain");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "failed save leaked its temporary file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
