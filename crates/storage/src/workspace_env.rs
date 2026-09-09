//! 워크스페이스별 API credential과 환경변수 이름의 연결. 비밀값은 저장하지 않는다.
use super::*;

const BINDINGS_MAX: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialEnvBinding {
    pub env_name: String,
    pub credential_id: String,
}

impl Db {
    pub fn validate_credential_env_name(name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            deppy_core::credential_env::valid_name(name),
            "credential_env_name_invalid"
        );
        Ok(())
    }

    fn write_credential_env_in_transaction(
        conn: &Connection,
        workspace: &str,
        credential: &str,
        name: Option<&str>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            workspace.len() <= 4096 && credential.len() <= 4096,
            "credential_env_identity_limit"
        );
        if let Some(name) = name {
            Self::validate_credential_env_name(name)?;
        }
        let allowed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM credentials WHERE id=?1
            AND (workspace_id IS NULL OR workspace_id=?2))",
            (credential, workspace),
            |row| row.get(0),
        )?;
        anyhow::ensure!(allowed, "credential_env_owner_invalid");
        conn.execute(
            "DELETE FROM workspace_credential_env WHERE workspace_id=?1 AND credential_id=?2",
            (workspace, credential),
        )?;
        if let Some(name) = name {
            conn.execute("INSERT INTO workspace_credential_env(workspace_id,env_name,credential_id) VALUES (?1,?2,?3)",
                (workspace, name, credential))?;
        }
        Ok(())
    }

    pub fn set_credential_env_binding(
        &self,
        workspace: &str,
        credential: &str,
        name: Option<&str>,
    ) -> anyhow::Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        Self::write_credential_env_in_transaction(&tx, workspace, credential, name)?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_credential_env_bindings(
        &self,
        workspace: &str,
    ) -> anyhow::Result<Vec<CredentialEnvBinding>> {
        let tx = self.conn.unchecked_transaction()?;
        let bindings = Self::credential_env_bindings_in_snapshot(&tx, workspace)?;
        tx.commit()?;
        Ok(bindings)
    }

    pub(super) fn credential_env_bindings_in_snapshot(
        conn: &Connection,
        workspace: &str,
    ) -> anyhow::Result<Vec<CredentialEnvBinding>> {
        let (count, row_bytes, invalid): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(length(CAST(b.env_name AS BLOB))+length(CAST(b.credential_id AS BLOB))),0),
             COALESCE(SUM(c.id IS NULL OR (c.workspace_id IS NOT NULL AND c.workspace_id<>b.workspace_id)),0)
             FROM workspace_credential_env b LEFT JOIN credentials c ON c.id=b.credential_id WHERE b.workspace_id=?1",
            [workspace], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        anyhow::ensure!(
            count <= BINDINGS_MAX as i64 && row_bytes <= 4352 && invalid == 0,
            "credential_env_snapshot_invalid"
        );
        let bindings = {
            let mut statement = conn.prepare("SELECT env_name,credential_id FROM workspace_credential_env WHERE workspace_id=?1 ORDER BY env_name LIMIT 257")?;
            statement
                .query_map([workspace], |row| {
                    Ok(CredentialEnvBinding {
                        env_name: row.get(0)?,
                        credential_id: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for binding in &bindings {
            Self::validate_credential_env_name(&binding.env_name)?;
        }
        Ok(bindings)
    }

    /// 메타데이터·slot 공개와 환경 연결을 같은 SQLite 트랜잭션으로 확정한다.
    pub fn insert_credential_with_env_slot(
        &self,
        meta: &CredentialMeta,
        slot: &str,
        env_name: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_owned_physical_secret_slot(&meta.id, slot)?;
        if let Some(name) = env_name {
            Self::validate_credential_env_name(name)?;
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        Self::insert_credential_with_secret_slot_in_transaction(&tx, meta, slot, None)?;
        if let Some(name) = env_name {
            let workspace = meta
                .workspace_id
                .as_deref()
                .context("credential_env_workspace_required")?;
            Self::write_credential_env_in_transaction(&tx, workspace, &meta.id, Some(name))?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(db: &Db, id: &str, workspace: Option<&str>) -> CredentialMeta {
        let meta = CredentialMeta {
            id: id.to_owned(),
            provider: "service".to_owned(),
            label: id.to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: None,
            workspace_id: workspace.map(str::to_owned),
        };
        db.insert_credential(&meta).unwrap();
        meta
    }
    #[test]
    fn credential_env_소유권과_이름충돌은_원래연결을_보존한다() {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_workspace("a").unwrap();
        let b = db.create_workspace("b").unwrap();
        key(&db, "a-key", Some(&a));
        key(&db, "b-key", Some(&b));
        key(&db, "shared", None);
        db.set_credential_env_binding(&a, "a-key", Some("SERVICE_KEY"))
            .unwrap();
        assert!(
            db.set_credential_env_binding(&a, "b-key", Some("OTHER_KEY"))
                .is_err()
        );
        db.set_credential_env_binding(&a, "shared", Some("SHARED_KEY"))
            .unwrap();
        assert!(
            db.set_credential_env_binding(&a, "shared", Some("SERVICE_KEY"))
                .is_err()
        );
        assert_eq!(db.list_credential_env_bindings(&a).unwrap().len(), 2);
        assert!(db.list_credential_env_bindings(&b).unwrap().is_empty());
        db.set_credential_env_binding(&a, "a-key", None).unwrap();
        assert!(!db.credential_in_use("a-key").unwrap());
        assert!(db.delete_credential_if_unused("a-key").unwrap());
        for invalid in ["", "1KEY", "BAD-NAME", "KEY=VALUE", "한글", "KEY\n"] {
            assert!(Db::validate_credential_env_name(invalid).is_err());
        }
        assert!(Db::validate_credential_env_name(&"A".repeat(257)).is_err());
    }
    #[test]
    fn credential_env_연결충돌은_신규_credential과_slot_공개를_롤백한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("a").unwrap();
        key(&db, "existing", Some(&workspace));
        db.set_credential_env_binding(&workspace, "existing", Some("SERVICE_KEY"))
            .unwrap();
        let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let plan = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), plan.new_slot().as_str())
            .unwrap();
        let meta = CredentialMeta {
            id: logical.as_str().to_owned(),
            provider: "service".into(),
            label: "new".into(),
            credential_kind: "api_key".into(),
            masked_hint: None,
            workspace_id: Some(workspace.clone()),
        };
        assert!(
            db.insert_credential_with_env_slot(
                &meta,
                plan.new_slot().as_str(),
                Some("SERVICE_KEY")
            )
            .is_err()
        );
        assert!(
            db.credential_secret_location(logical.as_str())
                .unwrap()
                .is_none()
        );
        db.insert_credential_with_env_slot(&meta, plan.new_slot().as_str(), Some("OTHER_KEY"))
            .unwrap();
        assert!(db.credential_in_use(logical.as_str()).unwrap());
        assert!(
            !db.delete_credential_if_unused_cas(logical.as_str(), plan.new_slot().as_str())
                .unwrap()
        );
    }
}
