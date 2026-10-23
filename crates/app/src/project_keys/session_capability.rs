//! A session bearer is inherited by the adapter, never serialized to model context.
use super::contracts::ProjectKeysError;
use super::repository::ProjectApiRepository;
use super::service::storage_error;
use deppy_core::project_api_access::{ProjectApiActorKind, ProjectApiSessionScope};
use secret::SecretString;
use sha2::{Digest as _, Sha256};

pub const CAPABILITY_ENV: &str = "DEPPY_PROJECT_API_CAPABILITY";

pub(super) fn capability_digest(bearer: &str) -> Result<[u8; 32], ProjectKeysError> {
    let payload = bearer
        .strip_prefix("deppy-api-")
        .ok_or(ProjectKeysError::Locked)?;
    if payload.len() != 64
        || !payload
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ProjectKeysError::Locked);
    }
    let mut hash = Sha256::new();
    hash.update(b"deppy-project-api-capability-v1\0");
    hash.update(bearer.as_bytes());
    Ok(hash.finalize().into())
}

pub fn issue_local_capability(
    db: &dyn ProjectApiRepository,
    actor: &ProjectApiSessionScope,
    expires_at: i64,
    at: i64,
) -> Result<SecretString, ProjectKeysError> {
    actor
        .validate()
        .map_err(|_| ProjectKeysError::InvalidInput)?;
    if actor.actor_kind != ProjectApiActorKind::LocalSession
        || at < 0
        || expires_at <= at
        || expires_at.saturating_sub(at) > 24 * 60 * 60
    {
        return Err(ProjectKeysError::InvalidInput);
    }
    // Two independently generated UUIDv4 values provide 244 random bits.
    let bearer = SecretString::new(format!(
        "deppy-api-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ));
    let digest = capability_digest(bearer.expose())?;
    storage_error(db.project_api_capability_save(actor, &digest, expires_at, at))?;
    Ok(bearer)
}

#[cfg(test)]
pub fn verify_local_capability(
    db: &dyn ProjectApiRepository,
    actor: &ProjectApiSessionScope,
    bearer: &str,
    at: i64,
) -> Result<(), ProjectKeysError> {
    let digest = capability_digest(bearer)?;
    if !storage_error(db.project_api_capability_allows(actor, &digest, at))? {
        return Err(ProjectKeysError::Locked);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_keys::tests::Fixture;
    use deppy_core::{project_api_access::ProjectApiActorKind, project_keys::KeyEnvironment};

    fn fixture() -> (Fixture, ProjectApiSessionScope) {
        let fixture = Fixture::new();
        let actor = ProjectApiSessionScope {
            workspace_id: fixture.scope.workspace_id.clone(),
            environment: KeyEnvironment::Development,
            principal_id: "local-agent".into(),
            session_id: "pane".into(),
            generation: "runtime-1-execution-1".into(),
            actor_kind: ProjectApiActorKind::LocalSession,
        };
        (fixture, actor)
    }

    #[test]
    fn project_api_capability_opaque_bearer_is_required_and_not_replayable_in_other_scope() {
        let (fixture, actor) = fixture();
        let db = &fixture.service.db;
        let bearer = issue_local_capability(db, &actor, 100, 10).unwrap();
        assert!(verify_local_capability(db, &actor, bearer.expose(), 11).is_ok());
        assert_eq!(
            verify_local_capability(db, &actor, "", 11),
            Err(ProjectKeysError::Locked)
        );
        let wrong = SecretString::new("not-a-session-capability".into());
        assert_eq!(
            verify_local_capability(db, &actor, wrong.expose(), 11),
            Err(ProjectKeysError::Locked)
        );
        let mut other = actor.clone();
        other.generation = "runtime-2-execution-1".into();
        assert_eq!(
            verify_local_capability(db, &other, bearer.expose(), 11),
            Err(ProjectKeysError::Locked)
        );
        other = actor.clone();
        other.workspace_id = db.create_workspace("Independent project").unwrap();
        assert_eq!(
            verify_local_capability(db, &other, bearer.expose(), 11),
            Err(ProjectKeysError::Locked)
        );
        assert_eq!(
            verify_local_capability(db, &actor, bearer.expose(), 100),
            Err(ProjectKeysError::Locked)
        );
        assert!(!format!("{bearer:?}").contains(bearer.expose()));
    }

    #[test]
    fn project_api_capability_rotation_and_revocation_reject_prior_bearers() {
        let (fixture, actor) = fixture();
        let db = &fixture.service.db;
        let first = issue_local_capability(db, &actor, 100, 10).unwrap();
        let second = issue_local_capability(db, &actor, 100, 11).unwrap();
        assert_ne!(first.expose(), second.expose());
        assert_eq!(
            verify_local_capability(db, &actor, first.expose(), 12),
            Err(ProjectKeysError::Locked)
        );
        assert!(verify_local_capability(db, &actor, second.expose(), 12).is_ok());
        db.project_api_capability_revoke_scope(&actor).unwrap();
        assert_eq!(
            verify_local_capability(db, &actor, second.expose(), 12),
            Err(ProjectKeysError::Locked)
        );
    }
}
