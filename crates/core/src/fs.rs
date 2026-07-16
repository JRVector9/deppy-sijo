//! 원자적 파일 기록 관례 (tmp + rename) — config/known_hosts/cert/아카이브 저장 공통.

use std::io;
use std::path::{Path, PathBuf};

/// `<파일명>.deppytmp`에 쓴 뒤 rename한다 — 쓰기 중 크래시로 빈/부분 파일이 대상
/// 경로에 남지 않는다 (rename은 동일 디렉터리 내에서 원자적). rename 실패 시 tmp를
/// 지운다. 부모 디렉터리 존재는 호출측이 보장한다.
///
/// suffix `.deppytmp`: ~/.claude, ~/.codex 같은 외부 도구와 공유하는 디렉터리에서
/// 남의 `.tmp`와 충돌하지 않게 우리 것임을 표시한다.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path(path)?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn tmp_path(path: &Path) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("파일명이 없는 경로: {}", path.display()),
        ));
    };
    let mut tmp = name.to_os_string();
    tmp.push(".deppytmp");
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
}
