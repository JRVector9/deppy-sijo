//! 종료 세션 스크롤백 압축 아카이브 (§14.3 확장 — A트랙 PR-A1).
//! 인메모리 아카이브(runtime)의 디스크 연장 — 워커 종료(suspend)·앱 재시작 후에도
//! 열람 복원(PR-A2)의 원천이 된다. logs.rs와 같은 계약: **호출측(runtime worker)이
//! redaction을 끝낸 바이트만 넘긴다** — 이 모듈은 평문 secret을 받지 않는다.
//!
//! 파일: `logs_root/<세션 UUID>/scrollback.zlib` (세션 로그와 같은 수명 정책)
//! 포맷: LE 고정 헤더 + zlib 스트림. 본문 무결성은 zlib(RFC 1950) adler32가 검증하고,
//! 헤더는 magic/version/필드 범위로 검증한다. 손상은 graceful skip(파일 삭제) —
//! 복원 실패가 앱 동작을 해치지 않는다. (헤더 메타 자급은 asciinema v2 관례 차용)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::logs::SessionLogWriter;

const MAGIC: &[u8; 4] = b"DPSA";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 2 + 4 + 1 + 4 + 4;
/// 압축 해제 크기 상한 — 오염/압축 폭탄 방어 (visible byte budget 16MB의 2배 여유)
const MAX_UNCOMPRESSED_BYTES: u32 = 32 * 1024 * 1024;
/// terminal.size sidecar와 동일한 grid 크기 상한 (범위 밖 = 오염 판정)
const MAX_GRID_DIM: u16 = 500;
/// 워크스페이스(logs_root)당 아카이브 총 바이트 예산 — 초과 시 mtime 오래된 것부터 GC
pub const ARCHIVE_DISK_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

const ARCHIVE_FILE: &str = "scrollback.zlib";

/// 복원에 필요한 세션 메타 — 파일 헤더에 자급한다 (sidecar 의존 없음).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveMeta {
    /// 0=shell, 1=agent (session::SessionKind 대응 — storage는 session에 의존하지 않는다)
    pub kind: u8,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: u32,
    pub exit_code: Option<u32>,
}

pub fn archive_path(logs_root: &Path, session_key: &str) -> anyhow::Result<PathBuf> {
    Ok(SessionLogWriter::session_dir_key(logs_root, session_key)?.join(ARCHIVE_FILE))
}

/// 이미 기록된 아카이브가 있는가 (exited grid는 불변 — 있으면 재기록하지 않는다).
pub fn exists(logs_root: &Path, session_key: &str) -> bool {
    archive_path(logs_root, session_key)
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// redaction이 끝난 ANSI 덤프를 압축해 원자적으로 기록한다 (tmp+rename).
pub fn write(
    logs_root: &Path,
    session_key: &str,
    meta: &ArchiveMeta,
    redacted_ansi: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        redacted_ansi.len() <= MAX_UNCOMPRESSED_BYTES as usize,
        "scrollback 아카이브 크기 초과: {} bytes",
        redacted_ansi.len()
    );
    let path = archive_path(logs_root, session_key)?;
    let dir = path.parent().expect("archive_path는 항상 부모가 있다");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("아카이브 디렉터리 생성 실패: {}", dir.display()))?;

    let mut buf = Vec::with_capacity(HEADER_LEN + redacted_ansi.len() / 4);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(meta.kind);
    buf.extend_from_slice(&meta.cols.to_le_bytes());
    buf.extend_from_slice(&meta.rows.to_le_bytes());
    buf.extend_from_slice(&meta.scrollback_lines.to_le_bytes());
    buf.push(meta.exit_code.is_some() as u8);
    buf.extend_from_slice(&meta.exit_code.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(&(redacted_ansi.len() as u32).to_le_bytes());
    let mut encoder = flate2::write::ZlibEncoder::new(buf, flate2::Compression::default());
    encoder
        .write_all(redacted_ansi)
        .context("scrollback 압축 실패")?;
    let bytes = encoder.finish().context("scrollback 압축 마감 실패")?;

    // 원자 기록 — 부분 파일이 노출되지 않는다 (프로젝트 관례: tmp+rename)
    let tmp = path.with_extension("zlib.tmp");
    std::fs::write(&tmp, &bytes).with_context(|| format!("tmp 기록 실패: {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("아카이브 rename 실패: {}", path.display())
    })?;
    Ok(())
}

/// 아카이브를 읽어 (메타, redacted ANSI 덤프)를 돌려준다.
/// 파일 없음 → Ok(None). 손상(헤더/범위/inflate 실패) → 파일 삭제 후 Ok(None)
/// (graceful skip — 복원 실패가 치명이 되지 않게).
pub fn read(logs_root: &Path, session_key: &str) -> anyhow::Result<Option<(ArchiveMeta, Vec<u8>)>> {
    let path = archive_path(logs_root, session_key)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("아카이브 읽기 실패: {}", path.display()));
        }
    };
    match parse(&bytes) {
        Some(parsed) => Ok(Some(parsed)),
        None => {
            tracing::warn!(path = %path.display(), "scrollback 아카이브 손상 — 폐기");
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }
}

fn parse(bytes: &[u8]) -> Option<(ArchiveMeta, Vec<u8>)> {
    if bytes.len() < HEADER_LEN || &bytes[0..4] != MAGIC || bytes[4] != VERSION {
        return None;
    }
    let kind = bytes[5];
    let cols = u16::from_le_bytes([bytes[6], bytes[7]]);
    let rows = u16::from_le_bytes([bytes[8], bytes[9]]);
    let scrollback_lines = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    let has_exit = bytes[14];
    let exit_code = u32::from_le_bytes([bytes[15], bytes[16], bytes[17], bytes[18]]);
    let uncompressed_len = u32::from_le_bytes([bytes[19], bytes[20], bytes[21], bytes[22]]);
    let valid = kind <= 1
        && (1..=MAX_GRID_DIM).contains(&cols)
        && (1..=MAX_GRID_DIM).contains(&rows)
        && has_exit <= 1
        && uncompressed_len <= MAX_UNCOMPRESSED_BYTES;
    if !valid {
        return None;
    }
    // take로 선언 길이 초과 해제를 차단 (압축 폭탄/오염 방어) — 정확 길이 검증까지
    let mut dump = Vec::with_capacity(uncompressed_len as usize);
    let mut decoder =
        flate2::read::ZlibDecoder::new(&bytes[HEADER_LEN..]).take(u64::from(uncompressed_len) + 1);
    if decoder.read_to_end(&mut dump).is_err() || dump.len() != uncompressed_len as usize {
        return None;
    }
    Some((
        ArchiveMeta {
            kind,
            cols,
            rows,
            scrollback_lines,
            exit_code: (has_exit == 1).then_some(exit_code),
        },
        dump,
    ))
}

/// logs_root 아래 아카이브 총량이 예산을 넘으면 mtime 오래된 것부터 삭제한다.
/// 로그 3종(redacted.*)은 건드리지 않는다 — 대상은 scrollback.zlib뿐.
pub fn gc(logs_root: &Path, budget_bytes: u64) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(logs_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("logs_root 나열 실패"),
    };
    let mut archives: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join(ARCHIVE_FILE);
        if let Ok(meta) = path.metadata() {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            archives.push((mtime, meta.len(), path));
        }
    }
    let mut total: u64 = archives.iter().map(|(_, len, _)| *len).sum();
    if total <= budget_bytes {
        return Ok(());
    }
    archives.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, len, path) in archives {
        if total <= budget_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total -= len;
                tracing::info!(path = %path.display(), "scrollback 아카이브 GC — 예산 초과 제거");
            }
            Err(e) => tracing::warn!(path = %path.display(), "아카이브 GC 삭제 실패: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "deppy-scrollback-archive-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta() -> ArchiveMeta {
        ArchiveMeta {
            kind: 1,
            cols: 120,
            rows: 40,
            scrollback_lines: 1_000,
            exit_code: Some(0),
        }
    }

    #[test]
    fn 라운드트립() {
        let root = temp_root();
        let dump = "한글 \x1b[31mred\x1b[0m line\r\nnext".as_bytes();
        write(&root, "uuid-1", &meta(), dump).unwrap();
        let (read_meta, read_dump) = read(&root, "uuid-1").unwrap().unwrap();
        assert_eq!(read_meta, meta());
        assert_eq!(read_dump, dump);
        // tmp 잔재 없음 (원자 기록)
        assert!(!root.join("uuid-1").join("scrollback.zlib.tmp").exists());
    }

    #[test]
    fn 파일_없음은_none() {
        assert!(read(&temp_root(), "missing").unwrap().is_none());
    }

    #[test]
    fn 손상_파일은_삭제_후_none() {
        let root = temp_root();
        // 압축이 잘 안 되는 payload — 절단이 확실히 deflate 본문을 자르게 한다
        // (동일 바이트 반복은 수십 바이트로 압축돼 트레일러만 잘릴 수 있음)
        let payload: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 251) as u8).collect();
        write(&root, "u", &meta(), &payload).unwrap();
        let path = archive_path(&root, "u").unwrap();
        // 본문 중간 절단
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(read(&root, "u").unwrap().is_none());
        assert!(!path.exists(), "손상 파일은 graceful skip으로 삭제");
        // magic 불일치
        std::fs::write(&path, b"XXXXjunkjunkjunkjunkjunkjunk").unwrap();
        assert!(read(&root, "u").unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn 경로_탈출_거부() {
        let root = temp_root();
        assert!(write(&root, "../evil", &meta(), b"x").is_err());
        assert!(read(&root, "a/b").is_err());
    }

    #[test]
    fn gc는_오래된_것부터_예산까지_제거하고_로그는_불가침() {
        let root = temp_root();
        let payload = vec![b'x'; 4096];
        for (i, key) in ["old", "mid", "new"].iter().enumerate() {
            write(&root, key, &meta(), &payload).unwrap();
            let path = archive_path(&root, key).unwrap();
            // mtime을 명시적으로 벌린다 (연속 기록의 mtime 해상도 문제 회피)
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000_000 + i as u64 * 1000);
            let file = std::fs::File::options().append(true).open(&path).unwrap();
            file.set_modified(time).unwrap();
        }
        // 같은 디렉터리의 로그 파일은 GC 대상이 아니다
        let log = root.join("old").join("redacted.ansi.log");
        std::fs::write(&log, b"log").unwrap();

        let one = archive_path(&root, "old")
            .unwrap()
            .metadata()
            .unwrap()
            .len();
        gc(&root, one * 2).unwrap(); // 3개 중 2개 예산 → 가장 오래된 1개 제거
        assert!(!archive_path(&root, "old").unwrap().exists());
        assert!(archive_path(&root, "mid").unwrap().exists());
        assert!(archive_path(&root, "new").unwrap().exists());
        assert!(log.exists(), "로그 파일 불가침");
    }
}
