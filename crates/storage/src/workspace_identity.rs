//! A volume proof is valid only for the exact stored path and inode anchor.
//! The workspaces UPDATE trigger invalidates proofs after explicit rebinding.
use super::{SETTINGS_ROW_BYTES_MAX, WorkspaceFolderAnchor};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

pub(super) fn verify(
    conn: &Connection,
    id: &str,
    path: &str,
    stored: Option<WorkspaceFolderAnchor>,
    current: WorkspaceFolderAnchor,
    volume: Option<uuid::Uuid>,
) -> anyhow::Result<()> {
    let Some(stored) = stored else {
        return Ok(());
    };
    let cached: Option<Option<String>> = conn.query_row(
        "SELECT CASE WHEN typeof(volume_uuid) = 'text' AND length(CAST(volume_uuid AS BLOB)) = 36
         THEN volume_uuid ELSE NULL END FROM workspace_volume_identities
         WHERE workspace_id=?1 AND path=?2 AND path_dev=?3 AND path_ino=?4",
        (id, path, stored.dev, stored.ino), |row| row.get(0),
    ).optional()?;
    let cached = cached
        .map(|value| -> anyhow::Result<_> {
            let value = value.context("workspace_volume_identity_invalid")?;
            let id = uuid::Uuid::parse_str(&value).context("workspace_volume_identity_invalid")?;
            anyhow::ensure!(!id.is_nil(), "workspace_volume_identity_invalid");
            Ok(id)
        })
        .transpose()?;
    anyhow::ensure!(stored.ino == current.ino, "workspace_path_anchor_conflict");
    if let Some(cached) = cached {
        anyhow::ensure!(volume == Some(cached), "workspace_path_anchor_conflict");
    } else {
        anyhow::ensure!(stored.dev == current.dev, "workspace_path_anchor_conflict");
    }
    Ok(())
}

pub(super) fn record(
    conn: &Connection,
    id: &str,
    path: &str,
    anchor: WorkspaceFolderAnchor,
    volume: Option<uuid::Uuid>,
) -> anyhow::Result<()> {
    if let Some(volume) = volume {
        anyhow::ensure!(
            id.len() + path.len() + 36 <= SETTINGS_ROW_BYTES_MAX,
            "workspace_volume_identity_bytes_limit"
        );
        anyhow::ensure!(!volume.is_nil(), "workspace_volume_identity_invalid");
        conn.execute(
            "INSERT INTO workspace_volume_identities(workspace_id,path,path_dev,path_ino,volume_uuid)
             VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workspace_id) DO UPDATE SET
             path=excluded.path,path_dev=excluded.path_dev,path_ino=excluded.path_ino,volume_uuid=excluded.volume_uuid",
            (id, path, anchor.dev, anchor.ino, volume.to_string()),
        )?;
    } else {
        conn.execute(
            "DELETE FROM workspace_volume_identities WHERE workspace_id=?1",
            [id],
        )?;
    }
    Ok(())
}
