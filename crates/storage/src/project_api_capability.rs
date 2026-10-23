//! Opaque local-session authorization. Only bearer digests are persisted.
use super::*;
use deppy_core::project_api_access::{ProjectApiActorKind, ProjectApiSessionScope};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE project_api_capabilities (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 environment TEXT NOT NULL CHECK(environment IN ('development','production')),
 principal_id TEXT NOT NULL, session_id TEXT NOT NULL, generation TEXT NOT NULL,
 bearer_hash BLOB NOT NULL CHECK(typeof(bearer_hash)='blob' AND length(bearer_hash)=32),
 expires_at INTEGER NOT NULL CHECK(expires_at>=0),
 PRIMARY KEY(workspace_id,environment,principal_id,session_id,generation)
);
CREATE INDEX idx_project_api_capability_expiry ON project_api_capabilities(expires_at);
"#;

impl Db {
    pub fn project_api_capability_save(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        expires_at: i64,
        now: i64,
    ) -> anyhow::Result<()> {
        scope.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            scope.actor_kind == ProjectApiActorKind::LocalSession,
            "project_api_capability_actor_invalid"
        );
        anyhow::ensure!(
            now >= 0 && expires_at > now && expires_at.saturating_sub(now) <= 24 * 60 * 60,
            "project_api_capability_expiry_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM project_api_capabilities WHERE expires_at<=?1",
            [now],
        )?;
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_api_capabilities WHERE workspace_id=?1 AND environment=?2 AND principal_id=?3 AND session_id=?4 AND generation=?5)",
            rusqlite::params![scope.workspace_id, scope.environment.as_str(), scope.principal_id, scope.session_id, scope.generation],
            |row| row.get(0),
        )?;
        if !existing {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM project_api_capabilities", [], |row| {
                    row.get(0)
                })?;
            anyhow::ensure!(count < 8192, "project_api_capability_capacity");
        }
        tx.execute(
            "INSERT INTO project_api_capabilities(workspace_id,environment,principal_id,session_id,generation,bearer_hash,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workspace_id,environment,principal_id,session_id,generation) DO UPDATE SET bearer_hash=excluded.bearer_hash,expires_at=excluded.expires_at",
            rusqlite::params![scope.workspace_id, scope.environment.as_str(), scope.principal_id, scope.session_id, scope.generation, bearer_hash.as_slice(), expires_at],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn project_api_capability_allows(
        &self,
        scope: &ProjectApiSessionScope,
        bearer_hash: &[u8; 32],
        now: i64,
    ) -> anyhow::Result<bool> {
        if scope.validate().is_err()
            || scope.actor_kind != ProjectApiActorKind::LocalSession
            || now < 0
        {
            return Ok(false);
        }
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_api_capabilities WHERE workspace_id=?1 AND environment=?2 AND principal_id=?3 AND session_id=?4 AND generation=?5 AND bearer_hash=?6 AND expires_at>?7)",
            rusqlite::params![scope.workspace_id, scope.environment.as_str(), scope.principal_id, scope.session_id, scope.generation, bearer_hash.as_slice(), now],
            |row| row.get(0),
        ).map_err(Into::into)
    }

    pub fn project_api_capability_revoke_scope(
        &self,
        scope: &ProjectApiSessionScope,
    ) -> anyhow::Result<()> {
        scope.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            scope.actor_kind == ProjectApiActorKind::LocalSession,
            "project_api_capability_actor_invalid"
        );
        self.conn.execute(
            "DELETE FROM project_api_capabilities WHERE workspace_id=?1 AND environment=?2 AND principal_id=?3 AND session_id=?4 AND generation=?5",
            rusqlite::params![scope.workspace_id, scope.environment.as_str(), scope.principal_id, scope.session_id, scope.generation],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deppy_core::project_keys::KeyEnvironment;

    fn fixture() -> (Db, ProjectApiSessionScope) {
        let db = Db::open_in_memory().unwrap();
        let workspace_id = db.create_workspace("Capability fixture").unwrap();
        let scope = ProjectApiSessionScope {
            workspace_id,
            environment: KeyEnvironment::Development,
            principal_id: "local-agent".into(),
            session_id: "same-pane-number".into(),
            generation: "runtime-1-execution-1".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        (db, scope)
    }

    #[test]
    fn project_api_capability_binds_full_actor_and_expires() {
        let (db, scope) = fixture();
        let digest = [7; 32];
        db.project_api_capability_save(&scope, &digest, 100, 10)
            .unwrap();
        assert!(
            db.project_api_capability_allows(&scope, &digest, 11)
                .unwrap()
        );
        assert!(
            !db.project_api_capability_allows(&scope, &[8; 32], 11)
                .unwrap()
        );
        for field in 0..6 {
            let mut other = scope.clone();
            match field {
                0 => other.workspace_id = db.create_workspace("Other project").unwrap(),
                1 => other.environment = KeyEnvironment::Production,
                2 => other.principal_id = "other-agent".into(),
                3 => other.session_id = "other-pane".into(),
                4 => other.generation = "runtime-2-execution-1".into(),
                _ => other.actor_kind = ProjectApiActorKind::CloudConnection,
            }
            assert!(
                !db.project_api_capability_allows(&other, &digest, 11)
                    .unwrap(),
                "scope field {field}"
            );
        }
        assert!(
            !db.project_api_capability_allows(&scope, &digest, 100)
                .unwrap()
        );
        assert!(
            !db.project_api_capability_allows(&scope, &digest, -1)
                .unwrap()
        );
        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT bearer_hash FROM project_api_capabilities",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, digest);
    }

    #[test]
    fn project_api_capability_reissue_and_revoke_leave_other_runtime_independent() {
        let (db, scope) = fixture();
        let mut other = scope.clone();
        other.generation = "runtime-2-execution-1".into();
        db.project_api_capability_save(&scope, &[7; 32], 100, 10)
            .unwrap();
        db.project_api_capability_save(&other, &[8; 32], 100, 10)
            .unwrap();
        db.project_api_capability_save(&scope, &[9; 32], 100, 11)
            .unwrap();
        assert!(
            !db.project_api_capability_allows(&scope, &[7; 32], 12)
                .unwrap()
        );
        assert!(
            db.project_api_capability_allows(&scope, &[9; 32], 12)
                .unwrap()
        );
        db.project_api_capability_revoke_scope(&scope).unwrap();
        assert!(
            !db.project_api_capability_allows(&scope, &[9; 32], 12)
                .unwrap()
        );
        assert!(
            db.project_api_capability_allows(&other, &[8; 32], 12)
                .unwrap()
        );
    }

    #[test]
    fn project_api_capability_rejects_cloud_and_unbounded_lifetime() {
        let (db, mut scope) = fixture();
        assert!(
            db.project_api_capability_save(&scope, &[7; 32], 10, 10)
                .is_err()
        );
        assert!(
            db.project_api_capability_save(&scope, &[7; 32], 86_411, 10)
                .is_err()
        );
        scope.actor_kind = ProjectApiActorKind::CloudConnection;
        assert!(
            db.project_api_capability_save(&scope, &[7; 32], 100, 10)
                .is_err()
        );
    }
}
