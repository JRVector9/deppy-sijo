use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

/// SQLite metadata DB (설계문서 11장). secret 평문은 절대 저장하지 않는다 —
/// credentials 행은 keyring 좌표와 masked_hint만 가진다 (6.3).
pub struct Db {
    conn: Connection,
}

/// user_version 기반 마이그레이션. PR-02는 credentials 테이블만 도입한다.
const MIGRATIONS: &[&str] = &["
CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    credential_kind TEXT NOT NULL,
    keyring_service TEXT NOT NULL,
    keyring_username TEXT NOT NULL,
    masked_hint TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_used_at TEXT
);
"];

#[derive(Debug, Clone, PartialEq)]
pub struct CredentialMeta {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("SQLite 열기 실패: {}", path.display()))?;
        Self::migrate(conn)
    }

    #[cfg(test)]
    fn open_in_memory() -> anyhow::Result<Self> {
        Self::migrate(Connection::open_in_memory()?)
    }

    fn migrate(mut conn: Connection) -> anyhow::Result<Self> {
        let version: usize =
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            // 스키마 변경과 user_version 갱신을 한 트랜잭션으로 묶어
            // 중단 시 절반만 적용된 상태를 막는다
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("마이그레이션 {} 실패", i + 1))?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()
                .with_context(|| format!("마이그레이션 {} 커밋 실패", i + 1))?;
        }
        Ok(Self { conn })
    }

    /// credential metadata 추가. created_at/updated_at은 SQLite가 UTC로 기록한다.
    pub fn insert_credential(&self, meta: &CredentialMeta) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO credentials
                   (id, provider, label, credential_kind,
                    keyring_service, keyring_username, masked_hint, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (
                    &meta.id,
                    &meta.provider,
                    &meta.label,
                    &meta.credential_kind,
                    crate::secret::KEYRING_SERVICE,
                    &meta.id, // keyring username = credential id
                    &meta.masked_hint,
                ),
            )
            .with_context(|| format!("credential 저장 실패: {}", meta.id))?;
        Ok(())
    }

    pub fn list_credentials(&self) -> anyhow::Result<Vec<CredentialMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, label, credential_kind, masked_hint
             FROM credentials ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CredentialMeta {
                id: row.get(0)?,
                provider: row.get(1)?,
                label: row.get(2)?,
                credential_kind: row.get(3)?,
                masked_hint: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete_credential(&self, id: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM credentials WHERE id = ?1", [id])
            .with_context(|| format!("credential 삭제 실패: {id}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> CredentialMeta {
        CredentialMeta {
            id: id.into(),
            provider: "anthropic".into(),
            label: "개인 키".into(),
            credential_kind: "api_key".into(),
            masked_hint: Some("****3456".into()),
        }
    }

    #[test]
    fn credential_crud_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        db.insert_credential(&sample("cred-2")).unwrap();
        let listed = db.list_credentials().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], sample("cred-1"));

        db.delete_credential("cred-1").unwrap();
        let listed = db.list_credentials().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "cred-2");
    }

    #[test]
    fn 마이그레이션은_멱등() {
        let dir = std::env::temp_dir().join(format!("deppy-sijo-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        {
            let db = Db::open(&path).unwrap();
            db.insert_credential(&sample("cred-1")).unwrap();
        }
        // 재오픈 시 기존 데이터 유지 + 마이그레이션 재실행 없음
        let db = Db::open(&path).unwrap();
        assert_eq!(db.list_credentials().unwrap().len(), 1);
        drop(db); // Windows: 파일 핸들을 닫아야 삭제 가능
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keyring_좌표가_기록된다() {
        let db = Db::open_in_memory().unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        let (service, username): (String, String) = db
            .conn
            .query_row(
                "SELECT keyring_service, keyring_username FROM credentials WHERE id = 'cred-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(service, crate::secret::KEYRING_SERVICE);
        assert_eq!(username, "cred-1");
    }
}
