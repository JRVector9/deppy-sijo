
#[path = "/Users/jr/Desktop/projects/deppy-sijo-performance/crates/storage-core/src/lib.rs"]
mod storage_core;
use rusqlite::Connection;
fn main() {
    let dir = std::path::PathBuf::from(format!("/private/tmp/deppy-backup-production-20261014-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("metadata.sqlite3");
    const OLD: &[&str] = &["CREATE TABLE sample(id INTEGER PRIMARY KEY,value TEXT);"];
    const NEW: &[&str] = &["CREATE TABLE sample(id INTEGER PRIMARY KEY,value TEXT);", "CREATE TABLE marker(id INTEGER PRIMARY KEY);"];
    let writer = storage_core::open_with_migrations(&path, OLD).unwrap();
    writer.execute("INSERT INTO sample VALUES(1,'before')", []).unwrap();
    writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN;").unwrap();
    let first: i64 = reader.query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0)).unwrap();
    assert_eq!(first, 1);
    writer.execute("INSERT INTO sample VALUES(2,'committed_after_reader')", []).unwrap();
    let start = std::time::Instant::now();
    let migrated = storage_core::open_with_migrations(&path, NEW).expect("production migrate succeeded despite incomplete backup");
    let elapsed = start.elapsed();
    let live: i64 = migrated.query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0)).unwrap();
    let backup = Connection::open(path.with_extension("sqlite3.bak")).unwrap();
    let saved: i64 = backup.query_row("SELECT COUNT(*) FROM sample", [], |r| r.get(0)).unwrap();
    let integrity: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    let version = storage_core::read_user_version(&migrated).unwrap();
    println!("path={} live_rows={live} backup_rows={saved} migrated_version={version} backup_integrity={integrity} elapsed_ms={}", dir.display(), elapsed.as_millis());
    reader.execute_batch("ROLLBACK;").unwrap();
    assert!(elapsed < std::time::Duration::from_secs(8));
    assert_eq!(saved, live, "successful pre-migration backup omitted a committed row");
}
