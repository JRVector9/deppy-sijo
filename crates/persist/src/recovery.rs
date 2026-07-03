//! Crash recovery (PR-14 완료 기준): lock file 중복 실행 방지 /
//! orphan session reconcile / log offset partial-write 보정.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fs2::FileExt;
use rusqlite::Connection;

use crate::repo::SESSION_STATUS_EXITED;

/// 중복 실행 방지 lock (OS advisory file lock — unix flock / windows LockFileEx).
/// 살아있는 동안 파일에 배타적 lock을 걸고, 진단용으로 PID를 기록한다.
///
/// PID file 방식과 달리 **크래시/강제 종료 시 OS가 lock을 자동 해제**하므로
/// stale lock이 남지 않는다 (플랫폼 무관 — Windows 포함). PID 재사용 오판도 없다.
/// lock 파일 자체는 재사용을 위해 남겨 두고, 획득 여부는 advisory lock으로만 판정한다.
pub struct LockFile {
    path: PathBuf,
    // File을 살려 두는 동안 advisory lock이 유지된다. drop되면 OS가 해제.
    _file: File,
}

impl LockFile {
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("lock 파일 열기 실패: {}", path.display()))?;

        // 비블로킹 배타 lock 시도. 이미 살아있는 인스턴스가 쥐고 있으면 WouldBlock.
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // 진단용으로 기존 holder PID를 읽어 메시지에 싣는다 (판정에는 안 쓴다)
                let holder = fs::read_to_string(path).unwrap_or_default();
                let holder = holder.trim();
                if holder.is_empty() {
                    bail!("이미 실행 중입니다 (lock: {})", path.display());
                }
                bail!(
                    "이미 실행 중입니다 (pid {holder}, lock: {})",
                    path.display()
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("lock 획득 실패: {}", path.display()));
            }
        }

        // 획득 성공 — 진단용 PID 기록 (이전 내용 덮어쓰기)
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(std::process::id().to_string().as_bytes()))
            .and_then(|()| file.flush())
            .with_context(|| format!("lock PID 기록 실패: {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
            _file: file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // advisory lock은 _file drop 시 OS가 해제한다. 파일은 **지우지 않는다** —
        // unix advisory lock은 inode 기준이라, unlink 후 새 프로세스가 같은 경로에
        // 새 inode로 lock을 잡으면 아직 살아있는 이 프로세스와 중복 실행이 가능해진다
        // (codex 리뷰 반영). 파일 잔존은 무해: 판정은 언제나 lock으로만 한다.
    }
}

/// 앱 시작 시 orphan reconcile: exited가 아닌 status(running / waiting /
/// needs_approval 등)는 모두 프로세스 생존을 전제하는데, 재시작 직후의 프로세스는
/// 이 앱이 spawn한 것이 아니므로 항상 orphan이다 → exited로 마킹.
/// 마킹한 행 수를 돌려준다.
pub fn reconcile_orphan_sessions(conn: &Connection) -> anyhow::Result<usize> {
    conn.execute(
        "UPDATE sessions SET status = ?1,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
         WHERE status != ?1",
        [SESSION_STATUS_EXITED],
    )
    .context("orphan session reconcile 실패")
}

/// 기록된 log offset을 실제 파일과 대조해 보정한다 (partial write 복구).
/// 파일이 없으면 0, offset이 파일 길이 이내면 그대로, 파일이 offset보다 짧으면
/// (crash로 offset 기록보다 로그가 덜 써진 것) 마지막 개행 직후 =
/// 마지막 완전한 라인의 끝(개행으로 끝나면 파일 길이)으로 되돌린다.
///
/// offset ≤ 길이일 때 라인 경계 정렬은 하지 않는다 — ansi/plain 로그(7장)는
/// 라인 지향이 아니라서 개행 정렬이 오히려 유효 offset을 훼손한다.
/// 잘림 판정 기준은 파일 길이다.
pub fn validate_log_offset(path: &Path, recorded_offset: u64) -> anyhow::Result<u64> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(e).with_context(|| format!("log 파일 열기 실패: {}", path.display()));
        }
    };
    let len = file
        .metadata()
        .with_context(|| format!("log 파일 metadata 실패: {}", path.display()))?
        .len();
    if recorded_offset <= len {
        return Ok(recorded_offset);
    }
    last_newline_end(&mut file, len)
        .with_context(|| format!("log 파일 개행 탐색 실패: {}", path.display()))
}

/// 파일 끝에서부터 chunk 단위로 거슬러 마지막 `\n`의 다음 위치를 찾는다. 없으면 0.
fn last_newline_end(file: &mut fs::File, len: u64) -> std::io::Result<u64> {
    const CHUNK: usize = 8192;
    let mut buf = [0u8; CHUNK];
    let mut end = len;
    while end > 0 {
        let start = end.saturating_sub(CHUNK as u64);
        let n = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use crate::repo::tests::{sample_session, sample_window, setup_schema, test_conn};
    use crate::repo::{
        SESSION_STATUS_EXITED, SESSION_STATUS_RUNNING, load_sessions, load_window_layouts,
        save_window_layout, upsert_session,
    };

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("deppy-sijo-persist-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_중복_획득_실패와_drop_해제() {
        let dir = temp_dir("lock");
        let path = dir.join("app.lock");

        let lock = LockFile::acquire(&path).unwrap();
        assert_eq!(lock.path(), path.as_path());
        // 같은 lock을 다시 잡으면 실패 (이 프로세스 PID가 살아있으므로)
        assert!(LockFile::acquire(&path).is_err());

        // drop 시 해제 → 재획득 가능
        drop(lock);
        let lock = LockFile::acquire(&path).unwrap();
        drop(lock);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_lock은_내용과_무관하게_재획득() {
        // advisory lock 방식: 이전 실행이 남긴 lock 파일은 (그 프로세스가 죽어
        // OS가 lock을 해제했으므로) 내용과 무관하게 즉시 다시 잡힌다 — 크래시로
        // 남은 오래된 PID/쓰다 만 파일 모두 정상 회수. Windows 포함 전 플랫폼 동일.
        let dir = temp_dir("stale");
        let path = dir.join("app.lock");

        for leftover in ["2147000000", "garbage", "0", ""] {
            std::fs::write(&path, leftover).unwrap();
            let lock = LockFile::acquire(&path).unwrap();
            // 획득하면 이 프로세스 PID로 갱신된다
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                std::process::id().to_string()
            );
            drop(lock);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_offset_partial_write_보정() {
        let dir = temp_dir("offset");
        let log = dir.join("events.redacted.jsonl");

        // 파일 없음 → 0
        assert_eq!(validate_log_offset(&log, 100).unwrap(), 0);

        // offset이 파일 길이 이내 → 그대로
        std::fs::write(&log, b"line1\nline2\n").unwrap();
        assert_eq!(validate_log_offset(&log, 6).unwrap(), 6);
        assert_eq!(validate_log_offset(&log, 12).unwrap(), 12);

        // 파일이 offset보다 짧고 개행으로 끝남 → 파일 길이
        assert_eq!(validate_log_offset(&log, 999).unwrap(), 12);

        // 파일이 offset보다 짧고 마지막 라인이 잘림 → 마지막 개행 직후
        std::fs::write(&log, b"line1\nlin").unwrap();
        assert_eq!(validate_log_offset(&log, 999).unwrap(), 6);

        // 개행이 아예 없는 잘린 파일 → 0
        std::fs::write(&log, b"partial").unwrap();
        assert_eq!(validate_log_offset(&log, 999).unwrap(), 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 개행_탐색은_chunk_경계를_넘는다() {
        let dir = temp_dir("chunk");
        let log = dir.join("big.log");
        // 개행 하나 뒤에 chunk(8192)보다 긴 잘린 라인
        let mut data = b"first line\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 20_000));
        std::fs::write(&log, &data).unwrap();
        assert_eq!(validate_log_offset(&log, u64::MAX).unwrap(), 11);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PR-14 완료 기준: 비정상 종료 후 재시작 시나리오.
    /// 저장 → (프로세스 사망 모사: 연결 drop, running 프로세스 소멸) →
    /// 새 연결로 load → reconcile → orphan은 exited, layout은 그대로.
    #[test]
    fn 비정상_종료_후_재시작_복원() {
        let dir = temp_dir("restart");
        let db_path = dir.join("metadata.sqlite3");
        let window = sample_window();

        // 1) 첫 실행: session running + layout 저장
        {
            let mut conn = Connection::open(&db_path).unwrap();
            setup_schema(&conn);
            upsert_session(&conn, &sample_session("sess-1", SESSION_STATUS_RUNNING)).unwrap();
            upsert_session(&conn, &sample_session("sess-2", SESSION_STATUS_EXITED)).unwrap();
            save_window_layout(&mut conn, "ws-1", &window).unwrap();
            // 비정상 종료 모사 — 정리 코드 없이 연결만 drop
        }

        // 2) 재시작: 새 연결로 복원
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();

        // running이던 sess-1은 프로세스가 없다 (재시작 후엔 항상 orphan) → reconcile
        let sessions = load_sessions(&conn, "ws-1").unwrap();
        assert_eq!(sessions[0].status, SESSION_STATUS_RUNNING);
        assert_eq!(reconcile_orphan_sessions(&conn).unwrap(), 1);
        let sessions = load_sessions(&conn, "ws-1").unwrap();
        assert!(sessions.iter().all(|s| s.status == SESSION_STATUS_EXITED));
        // 이미 exited였던 sess-2는 재실행해도 변화 없음 (멱등)
        assert_eq!(reconcile_orphan_sessions(&conn).unwrap(), 0);

        // layout은 저장 그대로 복원된다
        let loaded = load_window_layouts(&conn, "ws-1").unwrap();
        assert_eq!(loaded, vec![window]);

        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reconcile은_exited가_아닌_모든_status를_orphan으로_본다() {
        let conn = test_conn();
        upsert_session(&conn, &sample_session("sess-1", SESSION_STATUS_RUNNING)).unwrap();
        // waiting도 프로세스 생존 전제 → 재시작 후엔 orphan
        upsert_session(&conn, &sample_session("sess-2", "waiting")).unwrap();
        upsert_session(&conn, &sample_session("sess-3", SESSION_STATUS_EXITED)).unwrap();
        assert_eq!(reconcile_orphan_sessions(&conn).unwrap(), 2);
        let sessions = load_sessions(&conn, "ws-1").unwrap();
        assert!(sessions.iter().all(|s| s.status == SESSION_STATUS_EXITED));
    }
}
