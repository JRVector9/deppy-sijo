//! 워크스페이스별 API credential과 환경변수 이름의 연결. 비밀값은 저장하지 않는다.
use super::*;

const BINDINGS_MAX: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialEnvBinding {
    pub env_name: String,
    pub credential_id: String,
}

impl Db {
    pub fn default_env_source_files() -> Vec<String> {
        deppy_core::env_sources::default_files()
    }

    pub fn validate_env_source_files(files: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(
            deppy_core::env_sources::valid_files(files),
            "env_source_name_invalid"
        );
        Ok(())
    }

    pub fn set_env_source_files(
        &self,
        workspace: &str,
        files: Option<&[String]>,
    ) -> anyhow::Result<()> {
        if let Some(files) = files {
            Self::validate_env_source_files(files)?;
            let encoded = serde_json::to_string(files)?;
            self.conn.execute(
                "INSERT INTO workspace_env_sources(workspace_id,files_json) VALUES (?1,?2)
                ON CONFLICT(workspace_id) DO UPDATE SET files_json=excluded.files_json",
                (workspace, encoded),
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM workspace_env_sources WHERE workspace_id=?1",
                [workspace],
            )?;
        }
        Ok(())
    }

    pub fn env_source_files(&self, workspace: &str) -> anyhow::Result<Vec<String>> {
        Self::env_source_files_in_snapshot(&self.conn, workspace)
    }

    pub(super) fn env_source_files_in_snapshot(
        conn: &Connection,
        workspace: &str,
    ) -> anyhow::Result<Vec<String>> {
        // 큰 손상 행은 Rust 문자열로 복사하기 전에 차단한다.
        let row: Option<Option<String>> = conn.query_row("SELECT CASE WHEN length(CAST(files_json AS BLOB)) <= 8192 THEN files_json ELSE NULL END
            FROM workspace_env_sources WHERE workspace_id=?1", [workspace], |row| row.get(0)).optional()?;
        let files = match row {
            None => Self::default_env_source_files(),
            Some(Some(encoded)) => serde_json::from_str(&encoded)
                .map_err(|_| anyhow::anyhow!("env_sources_invalid"))?,
            Some(None) => anyhow::bail!("env_sources_limit"),
        };
        Self::validate_env_source_files(&files)?;
        Ok(files)
    }

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

    /// 해당 워크스페이스의 수동 API 메타와 slot만 읽는다. 비밀키 본문은 조회하지 않는다.
    pub fn workspace_api_credential(
        &self,
        workspace: &str,
        credential_id: &str,
    ) -> anyhow::Result<(CredentialMeta, CredentialSecretLocation)> {
        let tx = self.conn.unchecked_transaction()?;
        let location = Self::credential_secret_location_in_snapshot(&tx, credential_id)?
            .context("credential_edit_missing")?;
        let meta = Self::editable_api_credential(
            &tx,
            workspace,
            credential_id,
            &location.keyring_username,
        )?;
        tx.commit()?;
        Ok((meta, location))
    }

    fn editable_api_credential(
        conn: &Connection,
        workspace: &str,
        credential_id: &str,
        expected_slot: &str,
    ) -> anyhow::Result<CredentialMeta> {
        let meta = settings_credential_for_publish(conn, credential_id, expected_slot)?
            .context("credential_edit_stale_or_missing")?;
        anyhow::ensure!(
            meta.workspace_id.as_deref() == Some(workspace),
            "credential_edit_owner_invalid"
        );
        anyhow::ensure!(
            matches!(meta.credential_kind.as_str(), "api_key" | "token"),
            "credential_edit_kind_invalid"
        );
        let plain: bool = conn.query_row(
            "SELECT oauth_json IS NULL AND keyring_service=?2 FROM credentials WHERE id=?1",
            (credential_id, secret::KEYRING_SERVICE),
            |row| row.get(0),
        )?;
        anyhow::ensure!(plain, "credential_edit_oauth_or_service_invalid");
        Ok(meta)
    }

    /// 기존 ID를 유지하고 메타·환경 연결·선택적 키 교체를 원자적으로 확정한다.
    pub fn update_credential_with_env_slot(
        &self,
        workspace: &str,
        meta: &CredentialMeta,
        expected_slot: &str,
        replacement_slot: Option<&str>,
        env_name: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_owned_physical_secret_slot(&meta.id, expected_slot)?;
        anyhow::ensure!(
            meta.workspace_id.as_deref() == Some(workspace),
            "credential_edit_owner_invalid"
        );
        anyhow::ensure!(
            !meta.provider.trim().is_empty()
                && meta.provider.len() <= 4096
                && !meta.label.trim().is_empty()
                && meta.label.len() <= 4096
                && matches!(meta.credential_kind.as_str(), "api_key" | "token"),
            "credential_edit_metadata_invalid"
        );
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let original = Self::editable_api_credential(&tx, workspace, &meta.id, expected_slot)?;
        let mut candidate = meta.clone();
        if replacement_slot.is_none() {
            candidate.masked_hint = original.masked_hint;
        }
        settings_credential_candidate_write_admission(&tx, &candidate)?;
        Self::write_credential_env_in_transaction(&tx, workspace, &meta.id, env_name)?;
        tx.execute(
            "UPDATE credentials SET provider=?2, label=?3, credential_kind=?4,
             updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",
            (&meta.id, &meta.provider, &meta.label, &meta.credential_kind),
        )?;
        if let Some(slot) = replacement_slot {
            validate_owned_physical_secret_slot(&meta.id, slot)?;
            anyhow::ensure!(slot != expected_slot, "credential_edit_slot_reused");
            anyhow::ensure!(
                Self::publish_credential_secret_slot_in_transaction(
                    &tx,
                    &meta.id,
                    expected_slot,
                    slot,
                    None,
                    candidate.masked_hint.as_deref(),
                )?,
                "credential_edit_stale"
            );
        }
        tx.commit()?;
        Ok(())
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

    #[test]
    fn credential_edit_원래_id를_유지하고_메타와_연결과_slot을_함께_변경한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("edit").unwrap();
        let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let old = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), old.new_slot().as_str())
            .unwrap();
        let mut meta = CredentialMeta {
            id: logical.as_str().into(),
            provider: "test".into(),
            label: "before".into(),
            credential_kind: "api_key".into(),
            masked_hint: Some("old".into()),
            workspace_id: Some(workspace.clone()),
        };
        db.insert_credential_with_env_slot(&meta, old.new_slot().as_str(), Some("OLD_KEY"))
            .unwrap();
        meta.label = "renamed".into();
        db.update_credential_with_env_slot(
            &workspace,
            &meta,
            old.new_slot().as_str(),
            None,
            Some("NEW_KEY"),
        )
        .unwrap();
        assert_eq!(
            db.credential_secret_location(&meta.id)
                .unwrap()
                .unwrap()
                .keyring_username,
            old.new_slot().as_str()
        );
        let next =
            secret::SecretBundleStagePlan::allocate(logical.clone(), Some(old.new_slot().clone()))
                .unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), next.new_slot().as_str())
            .unwrap();
        meta.masked_hint = Some("new".into());
        db.update_credential_with_env_slot(
            &workspace,
            &meta,
            old.new_slot().as_str(),
            Some(next.new_slot().as_str()),
            None,
        )
        .unwrap();
        assert_eq!(db.list_credentials().unwrap(), vec![meta.clone()]);
        assert!(
            db.list_credential_env_bindings(&workspace)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.credential_secret_location(&meta.id)
                .unwrap()
                .unwrap()
                .keyring_username,
            next.new_slot().as_str()
        );
    }

    #[test]
    fn credential_edit_충돌과_다른_워크스페이스와_stale은_원본을_보존한다() {
        let db = Db::open_in_memory().unwrap();
        let workspace = db.create_workspace("edit").unwrap();
        let other = db.create_workspace("other").unwrap();
        key(&db, "taken", Some(&workspace));
        db.set_credential_env_binding(&workspace, "taken", Some("TAKEN"))
            .unwrap();
        let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let old = secret::SecretBundleStagePlan::allocate(logical.clone(), None).unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), old.new_slot().as_str())
            .unwrap();
        let original = CredentialMeta {
            id: logical.as_str().into(),
            provider: "test".into(),
            label: "before".into(),
            credential_kind: "api_key".into(),
            masked_hint: Some("old".into()),
            workspace_id: Some(workspace.clone()),
        };
        db.insert_credential_with_env_slot(&original, old.new_slot().as_str(), Some("ORIGINAL"))
            .unwrap();
        let next =
            secret::SecretBundleStagePlan::allocate(logical.clone(), Some(old.new_slot().clone()))
                .unwrap();
        db.register_physical_secret_slot_staging(logical.as_str(), next.new_slot().as_str())
            .unwrap();
        let mut changed = original.clone();
        changed.label = "after".into();
        changed.masked_hint = Some("new".into());
        for (owner, expected, name) in [
            (workspace.as_str(), old.new_slot().as_str(), "TAKEN"),
            (other.as_str(), old.new_slot().as_str(), "OTHER"),
            (workspace.as_str(), next.new_slot().as_str(), "OTHER"),
        ] {
            assert!(
                db.update_credential_with_env_slot(
                    owner,
                    &changed,
                    expected,
                    Some(next.new_slot().as_str()),
                    Some(name)
                )
                .is_err()
            );
            assert_eq!(
                db.list_credentials()
                    .unwrap()
                    .into_iter()
                    .find(|m| m.id == original.id)
                    .unwrap(),
                original
            );
            assert_eq!(
                db.credential_secret_location(&original.id)
                    .unwrap()
                    .unwrap()
                    .keyring_username,
                old.new_slot().as_str()
            );
            assert!(
                db.list_credential_env_bindings(&workspace)
                    .unwrap()
                    .iter()
                    .any(|b| b.credential_id == original.id && b.env_name == "ORIGINAL")
            );
        }
        // 메타와 환경 연결을 쓴 뒤 slot 공개가 실패해도 트랜잭션 전체가 복구된다.
        db.conn.execute_batch("CREATE TRIGGER reject_api_edit BEFORE UPDATE OF keyring_username ON credentials BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(
            db.update_credential_with_env_slot(
                &workspace,
                &changed,
                old.new_slot().as_str(),
                Some(next.new_slot().as_str()),
                Some("NEW_KEY")
            )
            .is_err()
        );
        assert_eq!(
            db.list_credentials()
                .unwrap()
                .into_iter()
                .find(|m| m.id == original.id)
                .unwrap(),
            original
        );
        assert_eq!(
            db.credential_secret_location(&original.id)
                .unwrap()
                .unwrap()
                .keyring_username,
            old.new_slot().as_str()
        );
        assert!(
            db.list_credential_env_bindings(&workspace)
                .unwrap()
                .iter()
                .any(|b| b.credential_id == original.id && b.env_name == "ORIGINAL")
        );
    }
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
    fn workspace_sources_순서와_중지와_격리를_저장한다() {
        let db = Db::open(
            &std::env::temp_dir().join(format!("deppy-sources-{}.sqlite3", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let a = db.create_workspace("a").unwrap();
        let b = db.create_workspace("b").unwrap();
        let files = vec![".env.local".into(), ".env.production".into()];
        db.set_env_source_files(&a, Some(&files)).unwrap();
        assert_eq!(db.env_source_files(&a).unwrap(), files);
        assert_eq!(
            db.env_source_files(&b).unwrap(),
            Db::default_env_source_files()
        );
        for invalid in [
            vec!["../.env".into()],
            vec!["/tmp/.env".into()],
            vec!["x/.env".into()],
            vec![".env".into(), ".env".into()],
            vec!["x".repeat(256)],
            vec!["a".into(); 17],
        ] {
            assert!(db.set_env_source_files(&a, Some(&invalid)).is_err());
            assert_eq!(db.env_source_files(&a).unwrap(), files);
        }
        db.set_env_source_files(&a, Some(&[])).unwrap();
        assert!(db.env_source_files(&a).unwrap().is_empty());
        db.set_env_source_files(&a, None).unwrap();
        assert_eq!(
            db.env_source_files(&a).unwrap(),
            Db::default_env_source_files()
        );
    }

    #[test]
    fn workspace_sources_선택_테이블이_존재한다() {
        let db = Db::open(
            &std::env::temp_dir().join(format!("deppy-sources-{}.sqlite3", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let exists: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workspace_env_sources')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn credential_env_소유권과_이름충돌은_원래연결을_보존한다() {
        let db = Db::open(
            &std::env::temp_dir().join(format!("deppy-sources-{}.sqlite3", uuid::Uuid::new_v4())),
        )
        .unwrap();
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
        let db = Db::open(
            &std::env::temp_dir().join(format!("deppy-sources-{}.sqlite3", uuid::Uuid::new_v4())),
        )
        .unwrap();
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
