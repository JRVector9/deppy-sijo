//! Crash recovery (PR-14 완료 기준): lock file 중복 실행 방지 /
//! orphan session reconcile / log offset partial-write 보정.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rusqlite::Connection;

use crate::repo::SESSION_STATUS_EXITED;

/// 중복 실행 방지 lock (PID file). 획득하면 파일에 이 프로세스의 PID가 기록되고
/// drop 시 파일이 지워진다. 죽은 PID가 남긴 stale lock은 회수한다.
///
/// 한계: PID file 방식이라 stale 판정(read)과 제거(remove) 사이에 짧은 경쟁 창이 있다.
/// create_new 재시도로 창을 좁히며, PID 재사용으로 무관한 프로세스가 그 PID를 쓰고
/// 있으면 실행 중으로 오판할 수 있다 (보수적 — 중복 실행 방지 우선).
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        for _ in 0..3 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    file.write_all(std::process::id().to_string().as_bytes())
                        .and_then(|()| file.sync_all())
                        .with_context(|| format!("lock PID 기록 실패: {}", path.display()))?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let holder = fs::read_to_string(path).unwrap_or_default();
                    match holder.trim().parse::<u32>() {
                        // pid 0은 유효한 holder가 아니다 (unix kill(0,·)은 프로세스
                        // 그룹 검사라 항상 성공 — 오판 방지 위해 stale로 취급)
                        Ok(pid) if pid != 0 && pid_alive(pid) => {
                            bail!("이미 실행 중입니다 (pid {pid}, lock: {})", path.display());
                        }
                        // 죽은 PID거나 파싱 불가(쓰다 만 파일) → stale, 회수 후 재시도
                        _ => {
                            tracing::warn!(lock = %path.display(), "stale lock 회수");
                            match fs::remove_file(path) {
                                Ok(()) => {}
                                // 다른 프로세스가 먼저 회수 — 재시도에서 판가름
                                Err(e) if e.kind() == ErrorKind::NotFound => {}
                                Err(e) => {
                                    return Err(e).with_context(|| {
                                        format!("stale lock 제거 실패: {}", path.display())
                                    });
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("lock 파일 생성 실패: {}", path.display()));
                }
            }
        }
        bail!("lock 획득 재시도 초과: {}", path.display())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            tracing::warn!(lock = %self.path.display(), "lock 해제 실패: {e}");
        }
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false; // pid_t 범위 밖 — 실존할 수 없는 PID
    };
    // signal 0: 시그널을 보내지 않고 존재만 검사. EPERM은 존재하지만 권한 없음 → 살아있음
    (unsafe { libc::kill(pid, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // 비-unix에는 판별 수단을 넣지 않았다 — 중복 실행 방지를 우선해 살아있다고 간주
    true
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

    #[cfg(unix)]
    #[test]
    fn stale_lock은_회수한다() {
        let dir = temp_dir("stale");
        let path = dir.join("app.lock");

        // 죽은 PID (pid 상한을 훨씬 넘는 값 — linux pid_max 최대 2^22)
        std::fs::write(&path, "2147000000").unwrap();
        let lock = LockFile::acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        drop(lock);

        // 쓰다 만(파싱 불가) lock도 stale로 회수
        std::fs::write(&path, "garbage").unwrap();
        drop(LockFile::acquire(&path).unwrap());

        // pid 0도 stale (kill(0,·)은 프로세스 그룹 검사라 생존 판정에 못 쓴다)
        std::fs::write(&path, "0").unwrap();
        drop(LockFile::acquire(&path).unwrap());
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
