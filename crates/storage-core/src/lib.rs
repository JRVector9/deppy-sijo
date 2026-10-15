//! storage-core — DB infra 전용 crate (v2.8 §6.1).
//!
//! SQLite 연결 열기(PRAGMA WAL/foreign_keys/busy_timeout), 마이그레이션 러너,
//! 마이그레이션 전 백업만 담당한다. **어떤 도메인 crate도 모른다**(xtask check-deps가
//! storage-core→도메인 edge를 금지 검사). 도메인 SQL/Row/Repo는 각 store crate 소유.
//!
//! 마이그레이션 원장 규칙(docs/dependency-graph.md): 호출자가 넘기는 `migrations`는
//! **전역 user_version 순서(v1..vN, 재배열 금지)** 의 SQL 목록이다. 이 러너는 순서를
//! 신뢰하고 index+1을 버전으로 쓴다 — store별 concat 재배열은 기존 DB를 깨뜨린다.

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

/// DB 파일을 열고(공통 PRAGMA 적용) pending 마이그레이션을 적용해 연결을 돌려준다.
///
/// - WAL + foreign_keys + busy_timeout(5s): 모든 연결 공통 (설계문서 11.9).
/// - pending migration이 있으면 적용 전 파일 백업(`*.sqlite3.bak`, 직전 1개 유지).
/// - 두 프로세스(GUI + deppy-mcp-proxy)가 동시에 열어도 안전: 결정+실행을 BEGIN
///   IMMEDIATE 한 트랜잭션으로 묶어, 두 번째 프로세스는 락 획득 후 최신 버전을 보고
///   no-op한다 (stale 계획으로 DDL 재실행하는 레이스 없음).
pub fn open_with_migrations(path: &Path, migrations: &[&str]) -> anyhow::Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("SQLite 열기 실패: {}", path.display()))?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    // 워커 persist 연결과 동시 쓰기가 겹칠 때 SQLITE_BUSY로 실패하지 않게 대기
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    migrate_with_backup(conn, migrations, Some(path)).with_context(|| {
        format!(
            "DB 마이그레이션 실패 — 백업: {}",
            path.with_extension("sqlite3.bak").display()
        )
    })
}

/// in-memory DB + 마이그레이션 (테스트용).
pub fn open_in_memory_with_migrations(migrations: &[&str]) -> anyhow::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(conn, migrations)
}

/// pending 마이그레이션을 하나의 IMMEDIATE 트랜잭션으로 적용한다.
fn migrate(conn: Connection, migrations: &[&str]) -> anyhow::Result<Connection> {
    migrate_with_backup(conn, migrations, None)
}

fn migrate_with_backup(
    mut conn: Connection,
    migrations: &[&str],
    path: Option<&Path>,
) -> anyhow::Result<Connection> {
    // 락 경합 최소화용 fast-path: 락 밖에서 한 번 읽어 이미 최신이면 트랜잭션 없이 리턴.
    // (여기 값은 참고용 — 실제 결정은 아래 IMMEDIATE 락 안에서 다시 읽어 확정한다.)
    let version = read_user_version(&conn)?;
    ensure_not_ahead(version, migrations.len())?;
    if version == migrations.len() {
        return Ok(conn);
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // 락 안에서 재확인 — 대기 중 다른 프로세스가 이미 올렸을 수 있다
    let version = read_user_version(&tx)?;
    ensure_not_ahead(version, migrations.len())?;
    if version > 0
        && version < migrations.len()
        && let Some(path) = path
    {
        // This IMMEDIATE transaction owns the writer lock. A separate read-only connection
        // sees the committed pre-migration snapshot, including WAL rows, while no other
        // process can race ahead and replace the pre-migration backup with a newer schema.
        backup_before_migration(path)?;
    }
    for (i, sql) in migrations.iter().enumerate().skip(version) {
        tx.execute_batch(sql)
            .with_context(|| format!("마이그레이션 {} 실패", i + 1))?;
    }
    if version < migrations.len() {
        // 스키마 변경 전부와 user_version 갱신이 같은 트랜잭션이라, 중간에 실패하면
        // user_version 포함 전부 롤백된다 (절반만 적용된 상태 방지)
        tx.pragma_update(None, "user_version", migrations.len() as i64)?;
    }
    tx.commit().context("마이그레이션 커밋 실패")?;
    Ok(conn)
}

/// Publish a completed standalone SQLite backup without touching the previous backup on failure.
fn backup_before_migration(path: &Path) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_BACKUP: AtomicU64 = AtomicU64::new(0);
    let backup_path = path.with_extension("sqlite3.bak");
    // SQLite NOFOLLOW rejects symlinks in every path component, including macOS /var.
    // Resolve only the directory so an existing backup symlink is replaced, never followed.
    let parent = backup_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let backup = std::fs::canonicalize(parent)
        .context("백업 디렉터리 확인 실패")?
        .join(backup_path.file_name().context("백업 파일명 없음")?);
    let mut temporary_name = backup
        .file_name()
        .context("백업 파일명 없음")?
        .to_os_string();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    temporary_name.push(format!(
        ".deppytmp-{}-{nanos}-{}",
        std::process::id(),
        NEXT_BACKUP.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = backup.with_file_name(temporary_name);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("백업 임시 파일 생성 실패")?;
    let result = (|| -> anyhow::Result<()> {
        file.set_permissions(std::fs::metadata(path)?.permissions())?;
        let source = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut destination = Connection::open_with_flags(
            &temporary,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        destination.pragma_update(None, "journal_mode", "DELETE")?;
        {
            let copy = rusqlite::backup::Backup::new(&source, &mut destination)?;
            anyhow::ensure!(
                copy.step(-1)? == rusqlite::backup::StepResult::Done,
                "SQLite 백업이 완료되지 않음"
            );
        }
        // The published .bak must restore using this main file alone, without WAL sidecars.
        destination.pragma_update(None, "journal_mode", "DELETE")?;
        destination.close().map_err(|(_, error)| error)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &backup)
            .with_context(|| format!("마이그레이션 전 백업 게시 실패: {}", backup.display()))?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("마이그레이션 전 백업 실패: {}", backup.display()))
}

/// 현재 user_version (적용된 마이그레이션 수).
pub fn read_user_version(conn: &Connection) -> anyhow::Result<usize> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize)
}

/// forward-only (11.9): 이 바이너리보다 앞선 DB는 downgrade가 불가능하므로
/// 조용히 실행하지 않고 기동을 중단한다.
fn ensure_not_ahead(version: usize, known: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        version <= known,
        "DB user_version({version})이 이 버전이 아는 마이그레이션({known})보다 앞서 있습니다 — \
         더 새 버전의 앱이 만든 DB입니다"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGS: &[&str] = &[
        "CREATE TABLE a (id TEXT PRIMARY KEY);",
        "CREATE TABLE b (id TEXT PRIMARY KEY, a_id TEXT, FOREIGN KEY(a_id) REFERENCES a(id));",
    ];

    #[test]
    fn 빈_db가_최신으로_올라간다() {
        let conn = open_in_memory_with_migrations(MIGS).unwrap();
        assert_eq!(read_user_version(&conn).unwrap(), 2);
    }

    #[test]
    fn 이미_최신이면_noop() {
        let dir = std::env::temp_dir().join(format!("storage-core-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite3");
        drop(open_with_migrations(&path, MIGS).unwrap());
        let conn = open_with_migrations(&path, MIGS).unwrap();
        assert_eq!(read_user_version(&conn).unwrap(), 2);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn 앞선_버전_db는_거부() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(migrate(conn, MIGS).is_err());
    }

    #[test]
    fn full_fix_20261014_backup_contains_committed_wal_rows_with_live_reader() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-backup-regression-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        const OLD: &[&str] = &["CREATE TABLE sample(id INTEGER PRIMARY KEY,value TEXT);"];
        const NEW: &[&str] = &[
            "CREATE TABLE sample(id INTEGER PRIMARY KEY,value TEXT);",
            "CREATE TABLE marker(id INTEGER PRIMARY KEY);",
        ];
        let writer = open_with_migrations(&path, OLD).unwrap();
        writer
            .execute("INSERT INTO sample VALUES(1,'before')", [])
            .unwrap();
        writer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN;").unwrap();
        let first: i64 = reader
            .query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, 1);
        writer
            .execute("INSERT INTO sample VALUES(2,'committed_after_reader')", [])
            .unwrap();
        let start = std::time::Instant::now();
        let migrated = open_with_migrations(&path, NEW)
            .expect("production migrate succeeded despite incomplete backup");
        let elapsed = start.elapsed();
        let live: i64 = migrated
            .query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0))
            .unwrap();
        let backup = Connection::open(path.with_extension("sqlite3.bak")).unwrap();
        let saved: i64 = backup
            .query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0))
            .unwrap();
        let integrity: String = backup
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        let version = read_user_version(&migrated).unwrap();
        println!(
            "path={} live_rows={live} backup_rows={saved} migrated_version={version} backup_integrity={integrity} elapsed_ms={}",
            dir.display(),
            elapsed.as_millis()
        );
        reader.execute_batch("ROLLBACK;").unwrap();
        drop(backup);
        drop(migrated);
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(elapsed < std::time::Duration::from_secs(8));
        assert_eq!(
            saved, live,
            "successful pre-migration backup omitted a committed row"
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_fix_20261014_backup_does_not_follow_previous_backup_symlink() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "deppy-backup-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        drop(open_with_migrations(&path, &MIGS[..1]).unwrap());
        let victim = dir.join("previous-backup-owner");
        std::fs::write(&victim, b"previous backup must survive").unwrap();
        symlink(&victim, path.with_extension("sqlite3.bak")).unwrap();
        let migrated = open_with_migrations(&path, MIGS).unwrap();
        let retained = std::fs::read(&victim).unwrap();
        let published_is_symlink = std::fs::symlink_metadata(path.with_extension("sqlite3.bak"))
            .unwrap()
            .file_type()
            .is_symlink();
        drop(migrated);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            retained, b"previous backup must survive",
            "backup followed an old backup link and modified its owner"
        );
        assert!(
            !published_is_symlink,
            "new backup must publish its own completed SQLite file"
        );
    }

    #[test]
    fn full_fix_20261014_backup_publication_failure_gates_migration_and_cleans_temp() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-backup-failure-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        drop(open_with_migrations(&path, &MIGS[..1]).unwrap());
        let backup = path.with_extension("sqlite3.bak");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("old-backup"), b"retain").unwrap();
        let failed = open_with_migrations(&path, MIGS);
        assert!(failed.is_err());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(read_user_version(&conn).unwrap(), 1);
        assert_eq!(std::fs::read(backup.join("old-backup")).unwrap(), b"retain");
        assert!(
            !std::fs::read_dir(&dir).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("deppytmp")),
            "failed backup must clean its exclusive temporary file"
        );
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn full_fix_20261014_backup_accepts_a_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "deppy-backup-parent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("physical")).unwrap();
        symlink(dir.join("physical"), dir.join("alias")).unwrap();
        let path = dir.join("alias/metadata.sqlite3");
        let old = open_with_migrations(&path, &MIGS[..1]).unwrap();
        old.execute("INSERT INTO a VALUES(17)", []).unwrap();
        drop(old);
        let migrated = open_with_migrations(&path, MIGS).unwrap();
        let backup = Connection::open(dir.join("physical/metadata.sqlite3.bak")).unwrap();
        assert_eq!(read_user_version(&backup).unwrap(), 1);
        assert_eq!(
            backup
                .query_row("SELECT id FROM a", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "17"
        );
        drop(backup);
        drop(migrated);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
