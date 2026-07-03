use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

use crate::env::EnvValue;

/// SQLite metadata DB (설계문서 11장). secret 평문은 절대 저장하지 않는다 —
/// credentials 행은 keyring 좌표와 masked_hint만 가진다 (6.3).
pub struct Db {
    conn: Connection,
}

/// user_version 기반 forward-only 마이그레이션 (설계문서 11.9).
/// 1: credentials (PR-02), 2: workspaces + env_profiles/env_vars (PR-03, 11.0/11.6)
const MIGRATIONS: &[&str] = &[
    "
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
",
    "
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE env_profiles (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',
    is_production INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE TABLE env_vars (
    id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    plain_value TEXT,
    credential_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(profile_id, key),
    CHECK (kind IN ('plain', 'secret')),
    CHECK (kind != 'secret' OR (plain_value IS NULL AND credential_id IS NOT NULL)),
    CHECK (kind != 'plain' OR credential_id IS NULL),
    FOREIGN KEY(profile_id) REFERENCES env_profiles(id),
    FOREIGN KEY(credential_id) REFERENCES credentials(id)
);

CREATE INDEX idx_env_vars_profile_key ON env_vars(profile_id, key);
",
];

#[derive(Debug, Clone, PartialEq)]
pub struct CredentialMeta {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnvProfileRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_production: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnvVarRow {
    pub key: String,
    pub value: EnvValue,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("SQLite 열기 실패: {}", path.display()))?;
        // 설계문서 11.9: 모든 연결에 WAL + foreign_keys 강제
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;

        // 설계문서 11.9: pending migration이 있으면 적용 전 파일 백업 (직전 1개 유지)
        let version: usize =
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        if version > 0 && version < MIGRATIONS.len() {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            let backup = path.with_extension("sqlite3.bak");
            std::fs::copy(path, &backup)
                .with_context(|| format!("마이그레이션 전 백업 실패: {}", backup.display()))?;
        }
        Self::migrate(conn).with_context(|| {
            format!(
                "DB 마이그레이션 실패 — 백업: {}",
                path.with_extension("sqlite3.bak").display()
            )
        })
    }

    #[cfg(test)]
    fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Self::migrate(conn)
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

    /// env var가 이 credential을 참조 중인지 확인 (삭제 전 검사용).
    pub fn credential_in_use(&self, id: &str) -> anyhow::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM env_vars WHERE credential_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 기본 workspace를 보장하고 id를 돌려준다. 실제 workspace 관리는 PR-14.
    pub fn ensure_default_workspace(&self) -> anyhow::Result<String> {
        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM workspaces ORDER BY created_at LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO workspaces (id, name, path, created_at, updated_at)
             VALUES (?1, 'default', '',
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            [&id],
        )?;
        Ok(id)
    }

    /// env profile 생성. is_production은 kind에서 파생한다 (설계문서 6.4).
    pub fn insert_env_profile(
        &self,
        workspace_id: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .execute(
                "INSERT INTO env_profiles (id, workspace_id, name, kind, is_production, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                (&id, workspace_id, name, kind, kind == "production"),
            )
            .with_context(|| format!("env profile 저장 실패: {name}"))?;
        Ok(id)
    }

    pub fn list_env_profiles(&self, workspace_id: &str) -> anyhow::Result<Vec<EnvProfileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, kind, is_production FROM env_profiles
             WHERE workspace_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(EnvProfileRow {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                is_production: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// profile과 소속 env var를 한 트랜잭션으로 삭제한다.
    pub fn delete_env_profile(&mut self, id: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM env_vars WHERE profile_id = ?1", [id])?;
        tx.execute("DELETE FROM env_profiles WHERE id = ?1", [id])?;
        tx.commit()
            .with_context(|| format!("env profile 삭제 실패: {id}"))
    }

    /// env var 추가/갱신. secret은 credential_id만 저장한다 (설계문서 6.3) —
    /// EnvValue 타입 + DDL CHECK로 이중 강제.
    pub fn upsert_env_var(
        &self,
        profile_id: &str,
        key: &str,
        value: &EnvValue,
    ) -> anyhow::Result<()> {
        let (kind, plain_value, credential_id) = match value {
            EnvValue::Plain(v) => ("plain", Some(v.as_str()), None),
            EnvValue::Secret { credential_id } => ("secret", None, Some(credential_id.as_str())),
        };
        self.conn
            .execute(
                "INSERT INTO env_vars (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                    strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))
                 ON CONFLICT(profile_id, key) DO UPDATE SET
                    kind = excluded.kind,
                    plain_value = excluded.plain_value,
                    credential_id = excluded.credential_id,
                    updated_at = excluded.updated_at",
                (
                    uuid::Uuid::new_v4().to_string(),
                    profile_id,
                    key,
                    kind,
                    plain_value,
                    credential_id,
                ),
            )
            .with_context(|| format!("env var 저장 실패: {key}"))?;
        Ok(())
    }

    pub fn list_env_vars(&self, profile_id: &str) -> anyhow::Result<Vec<EnvVarRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, kind, plain_value, credential_id FROM env_vars
             WHERE profile_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map([profile_id], |row| {
            let key: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let value = if kind == "secret" {
                EnvValue::Secret {
                    credential_id: row.get(3)?,
                }
            } else {
                EnvValue::Plain(row.get::<_, Option<String>>(2)?.unwrap_or_default())
            };
            Ok(EnvVarRow { key, value })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete_env_var(&self, profile_id: &str, key: &str) -> anyhow::Result<()> {
        self.conn
            .execute(
                "DELETE FROM env_vars WHERE profile_id = ?1 AND key = ?2",
                [profile_id, key],
            )
            .with_context(|| format!("env var 삭제 실패: {key}"))?;
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
    fn env_profile과_var_roundtrip() {
        let mut db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        // 멱등: 재호출 시 같은 workspace
        assert_eq!(db.ensure_default_workspace().unwrap(), ws);

        let local = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        let prod = db.insert_env_profile(&ws, "운영", "production").unwrap();
        let profiles = db.list_env_profiles(&ws).unwrap();
        assert_eq!(profiles.len(), 2);
        let by_id = |id: &str| profiles.iter().find(|p| p.id == id).unwrap();
        assert!(!by_id(&local).is_production);
        assert!(by_id(&prod).is_production); // kind에서 파생

        db.insert_credential(&sample("cred-1")).unwrap();
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("3000".into()))
            .unwrap();
        db.upsert_env_var(
            &local,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "cred-1".into(),
            },
        )
        .unwrap();
        let vars = db.list_env_vars(&local).unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(
            vars[0],
            EnvVarRow {
                key: "API_KEY".into(),
                value: EnvValue::Secret {
                    credential_id: "cred-1".into()
                }
            }
        );

        // upsert: 같은 key 갱신
        db.upsert_env_var(&local, "PORT", &EnvValue::Plain("8080".into()))
            .unwrap();
        let vars = db.list_env_vars(&local).unwrap();
        assert_eq!(vars[1].value, EnvValue::Plain("8080".into()));

        db.delete_env_var(&local, "PORT").unwrap();
        assert_eq!(db.list_env_vars(&local).unwrap().len(), 1);

        db.delete_env_profile(&local).unwrap();
        assert_eq!(db.list_env_profiles(&ws).unwrap().len(), 1);
        assert_eq!(db.list_env_profiles(&ws).unwrap()[0].id, prod);
    }

    #[test]
    fn secret_var는_존재하는_credential만_참조한다() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        // FK 강제(설계문서 11.9): 없는 credential_id는 거부
        assert!(
            db.upsert_env_var(
                &profile,
                "API_KEY",
                &EnvValue::Secret {
                    credential_id: "cred-none".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn env_var가_참조하는_credential은_삭제_거부() {
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        db.insert_credential(&sample("cred-1")).unwrap();
        db.upsert_env_var(
            &profile,
            "API_KEY",
            &EnvValue::Secret {
                credential_id: "cred-1".into(),
            },
        )
        .unwrap();
        // 참조 검사 + FK가 참조 중인 credential 삭제를 막는다 (keyring drift 방지의 전제)
        assert!(db.credential_in_use("cred-1").unwrap());
        assert!(db.delete_credential("cred-1").is_err());
        db.delete_env_var(&profile, "API_KEY").unwrap();
        assert!(!db.credential_in_use("cred-1").unwrap());
        assert!(db.delete_credential("cred-1").is_ok());
    }

    #[test]
    fn ddl_check가_secret_평문을_거부한다() {
        // repository 타입(EnvValue)으로는 표현 불가한 조합을 raw SQL로 검증 (이중 강제)
        let db = Db::open_in_memory().unwrap();
        let ws = db.ensure_default_workspace().unwrap();
        let profile = db.insert_env_profile(&ws, "로컬", "local").unwrap();
        let result = db.conn.execute(
            "INSERT INTO env_vars (id, profile_id, key, kind, plain_value, credential_id, created_at, updated_at)
             VALUES ('v1', ?1, 'API_KEY', 'secret', 'leaked-plaintext', NULL, '', '')",
            [&profile],
        );
        assert!(result.is_err());
    }

    #[test]
    fn 마이그레이션_전_백업이_생성된다() {
        let dir = std::env::temp_dir().join(format!("deppy-sijo-bak-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.sqlite3");
        // 버전 1(credentials만)인 구버전 DB를 만든다
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(path.with_extension("sqlite3.bak").exists());
        // 마이그레이션 완료 후 env 테이블 사용 가능
        db.ensure_default_workspace().unwrap();
        drop(db);
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
