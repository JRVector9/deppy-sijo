//! Opt-in Command Center persistence. This store never opens the legacy metadata database.
//! The host validates existing workspace IDs and owns the worker/feature gate. Restoring this
//! snapshot restores data only: dispatch receipts and historical runtime bindings never execute
//! or resend an operation. The latest bounded event display page is retained with the project.

use std::path::Path;

use anyhow::Context;
use deppy_core::WorkspaceId;
use deppy_core::work_management::{
    Attempt, AttemptId, EventId, Goal, OperationId, PersistentBinding, ProjectConfig, Task, TaskId,
    WORK_DRAFT_MAX_BYTES, WORK_DRAFT_MAX_COUNT, WORK_DRAFT_TOTAL_MAX_BYTES, WORK_EVENT_MAX_BYTES,
    WORK_PAGE_MAX_ROWS, WORK_PLAN_MAX_TASKS, WorkPlan,
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};

#[path = "work_management_remote.rs"]
mod remote_operations;
pub use remote_operations::{RemoteOperationClaim, RemoteOperationIdentity, RemoteOperationResult};
#[path = "work_devices.rs"]
mod devices;
pub use devices::{RegisteredExecutionDevice, StoredExecutionDevice};

pub const COMMAND_CENTER_FILE_NAME: &str = "command-center-v2.sqlite3";

const APPLICATION_ID: i64 = 0x4443_4332;
const SCHEMA_VERSION: i64 = 3;
const DEVICE_SCHEMA: &str = r#"
CREATE TABLE work_execution_devices (
 workspace_id TEXT NOT NULL CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 128),
 device_id TEXT NOT NULL CHECK(length(CAST(device_id AS BLOB)) BETWEEN 1 AND 128),
 revision INTEGER NOT NULL CHECK(revision>0),
 body BLOB NOT NULL CHECK(typeof(body)='blob' AND length(body)<=32768),
 PRIMARY KEY(workspace_id,device_id)
);
"#;
const REMOTE_SCHEMA: &str = r#"
CREATE TABLE work_remote_operations (
 workspace_id TEXT NOT NULL CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 128),
 operation_id TEXT NOT NULL CHECK(length(CAST(operation_id AS BLOB)) BETWEEN 1 AND 128),
 attempt_id TEXT NOT NULL CHECK(length(CAST(attempt_id AS BLOB)) BETWEEN 1 AND 128),
 identity BLOB NOT NULL CHECK(typeof(identity)='blob' AND length(identity)<=2048),
 result BLOB CHECK(result IS NULL OR (typeof(result)='blob' AND length(result)<=1024)),
 PRIMARY KEY(workspace_id,operation_id), UNIQUE(workspace_id,attempt_id)
);
"#;
fn source_digest(version: i64) -> String {
    let mut hash = Sha256::new();
    hash.update(SCHEMA.as_bytes());
    if version >= 2 {
        hash.update(REMOTE_SCHEMA.as_bytes());
    }
    if version >= 3 {
        hash.update(DEVICE_SCHEMA.as_bytes());
    }
    digest_hex(&hash.finalize())
}
const SCHEMA: &str = r#"
CREATE TABLE work_store_meta (id INTEGER PRIMARY KEY CHECK(id=1), format TEXT NOT NULL, schema_digest TEXT NOT NULL, source_digest TEXT NOT NULL);
INSERT INTO work_store_meta VALUES(1, 'deppy-command-center-v2', '', '');
CREATE TABLE work_project_configs (
    workspace_id TEXT PRIMARY KEY CHECK(length(CAST(workspace_id AS BLOB)) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK(revision > 0),
    config_revision INTEGER NOT NULL CHECK(config_revision > 0),
    snapshot BLOB NOT NULL CHECK(typeof(snapshot)='blob' AND length(snapshot) <= 41943040)
);
CREATE TABLE work_goals (
    workspace_id TEXT NOT NULL, goal_id TEXT NOT NULL, submission_id TEXT NOT NULL,
    PRIMARY KEY(workspace_id, goal_id), UNIQUE(workspace_id, submission_id),
    FOREIGN KEY(workspace_id) REFERENCES work_project_configs(workspace_id) ON DELETE CASCADE
);
CREATE TABLE work_tasks (
    workspace_id TEXT NOT NULL, task_id TEXT NOT NULL, goal_id TEXT NOT NULL,
    parent_task_id TEXT, current_attempt_id TEXT,
    PRIMARY KEY(workspace_id, task_id), UNIQUE(workspace_id, goal_id, task_id),
    CHECK(parent_task_id IS NULL OR parent_task_id <> task_id),
    FOREIGN KEY(workspace_id, goal_id) REFERENCES work_goals(workspace_id, goal_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, goal_id, parent_task_id) REFERENCES work_tasks(workspace_id, goal_id, task_id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY(workspace_id, current_attempt_id, task_id) REFERENCES work_attempts(workspace_id, attempt_id, task_id) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE work_attempts (
    workspace_id TEXT NOT NULL, attempt_id TEXT NOT NULL, task_id TEXT NOT NULL,
    number INTEGER NOT NULL CHECK(number > 0), operation_id TEXT NOT NULL,
    PRIMARY KEY(workspace_id, attempt_id), UNIQUE(workspace_id, task_id, number),
    UNIQUE(workspace_id, operation_id), UNIQUE(workspace_id, attempt_id, operation_id),
    UNIQUE(workspace_id, attempt_id, task_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES work_tasks(workspace_id, task_id) ON DELETE CASCADE
);
CREATE TABLE work_dependencies (
    workspace_id TEXT NOT NULL, task_id TEXT NOT NULL, required_task_id TEXT NOT NULL,
    required_result_revision INTEGER NOT NULL CHECK(required_result_revision > 0),
    PRIMARY KEY(workspace_id, task_id, required_task_id), CHECK(task_id <> required_task_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES work_tasks(workspace_id, task_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, required_task_id) REFERENCES work_tasks(workspace_id, task_id) ON DELETE CASCADE
);
CREATE TRIGGER work_dependency_cycle BEFORE INSERT ON work_dependencies BEGIN
    SELECT RAISE(ABORT, 'dependency cycle') WHERE EXISTS (
        WITH RECURSIVE ancestors(id) AS (
            SELECT NEW.required_task_id UNION
            SELECT d.required_task_id FROM work_dependencies d JOIN ancestors a ON d.task_id=a.id WHERE d.workspace_id=NEW.workspace_id
        ) SELECT 1 FROM ancestors WHERE id=NEW.task_id
    );
END;
CREATE TABLE work_events (
    workspace_id TEXT NOT NULL, event_id TEXT NOT NULL, task_id TEXT, attempt_id TEXT,
    PRIMARY KEY(workspace_id, event_id), CHECK(attempt_id IS NULL OR task_id IS NOT NULL),
    FOREIGN KEY(workspace_id) REFERENCES work_project_configs(workspace_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, task_id) REFERENCES work_tasks(workspace_id, task_id) ON DELETE CASCADE,
    FOREIGN KEY(workspace_id, attempt_id, task_id) REFERENCES work_attempts(workspace_id, attempt_id, task_id) ON DELETE CASCADE
);
CREATE TABLE work_dispatch_outbox (
    workspace_id TEXT NOT NULL, attempt_id TEXT NOT NULL, operation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending','admitted','acknowledged','uncertain')),
    PRIMARY KEY(workspace_id, attempt_id, operation_id),
    FOREIGN KEY(workspace_id, attempt_id, operation_id) REFERENCES work_attempts(workspace_id, attempt_id, operation_id) ON DELETE CASCADE
);
"#;

pub const WORK_PROJECT_SNAPSHOT_MAX_BYTES: usize = 40 * 1024 * 1024;
pub const WORK_PROJECT_MAX_GOALS: usize = 64;
pub const WORK_PROJECT_MAX_ATTEMPTS: usize = 256;
pub const WORK_PROJECT_MAX_OUTBOX: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectRevision(u64);

impl ProjectRevision {
    pub fn new(value: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            value > 0 && value <= i64::MAX as u64,
            "invalid project revision"
        );
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDraft {
    pub target_key: String,
    pub text: String,
    pub delivery_uncertain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: EventId,
    pub task_id: Option<TaskId>,
    pub attempt_id: Option<AttemptId>,
    pub display_payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchOutboxState {
    Pending,
    Admitted,
    Acknowledged,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchOutbox {
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub state: DispatchOutboxState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCenterSnapshot {
    pub config: ProjectConfig,
    pub main_binding: Option<PersistentBinding>,
    #[serde(deserialize_with = "bounded_goals")]
    pub goals: Vec<Goal>,
    #[serde(deserialize_with = "bounded_tasks")]
    pub tasks: Vec<Task>,
    #[serde(deserialize_with = "bounded_attempts")]
    pub attempts: Vec<Attempt>,
    #[serde(deserialize_with = "bounded_events")]
    pub events: Vec<StoredEvent>,
    #[serde(deserialize_with = "bounded_outbox")]
    pub outbox: Vec<DispatchOutbox>,
    #[serde(deserialize_with = "bounded_drafts")]
    pub drafts: Vec<WorkDraft>,
}

fn bounded_vec<'de, D, T, const MAX: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T, const MAX: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::Visitor<'de> for Visitor<T, MAX> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "at most {MAX} records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut records = Vec::new();
            while let Some(record) = seq.next_element()? {
                if records.len() == MAX {
                    return Err(serde::de::Error::custom("snapshot row limit exceeded"));
                }
                records.push(record);
            }
            Ok(records)
        }
    }
    d.deserialize_seq(Visitor::<T, MAX>(std::marker::PhantomData))
}

macro_rules! bounded_records {
    ($name:ident, $type:ty, $max:expr) => {
        fn $name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<$type>, D::Error> {
            bounded_vec::<D, $type, $max>(d)
        }
    };
}
bounded_records!(bounded_goals, Goal, WORK_PROJECT_MAX_GOALS);
bounded_records!(bounded_tasks, Task, WORK_PLAN_MAX_TASKS);
bounded_records!(bounded_attempts, Attempt, WORK_PROJECT_MAX_ATTEMPTS);
bounded_records!(bounded_events, StoredEvent, WORK_PAGE_MAX_ROWS);
bounded_records!(bounded_outbox, DispatchOutbox, WORK_PROJECT_MAX_OUTBOX);
bounded_records!(bounded_drafts, WorkDraft, WORK_DRAFT_MAX_COUNT);

fn valid_id(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty() && value.len() <= 128,
        "invalid persisted identity"
    );
    Ok(())
}

impl CommandCenterSnapshot {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.config.validate()?;
        anyhow::ensure!(
            self.config.revision.get() <= i64::MAX as u64,
            "config revision exceeds SQLite range"
        );
        anyhow::ensure!(
            self.goals.len() <= WORK_PROJECT_MAX_GOALS
                && self.tasks.len() <= WORK_PLAN_MAX_TASKS
                && self.attempts.len() <= WORK_PROJECT_MAX_ATTEMPTS
                && self.events.len() <= WORK_PAGE_MAX_ROWS
                && self.outbox.len() <= WORK_PROJECT_MAX_OUTBOX
                && self.drafts.len() <= WORK_DRAFT_MAX_COUNT,
            "snapshot row limit exceeded"
        );
        if let Some(binding) = &self.main_binding {
            validate_binding(binding)?;
        }
        let workspace_id = &self.config.workspace_id;
        let mut goals = HashMap::new();
        let mut submissions = HashSet::new();
        for goal in &self.goals {
            goal.validate()?;
            anyhow::ensure!(
                &goal.workspace_id == workspace_id
                    && goals.insert(&goal.id, goal).is_none()
                    && submissions.insert(&goal.submission_id),
                "goal ownership or submission identity mismatch"
            );
        }
        let mut tasks = HashMap::new();
        for task in &self.tasks {
            task.validate()?;
            anyhow::ensure!(
                &task.workspace_id == workspace_id
                    && goals.contains_key(&task.goal_id)
                    && tasks.insert(&task.id, task).is_none(),
                "task ownership mismatch"
            );
            if let Some(result) = &task.result {
                anyhow::ensure!(
                    result.revision.get() <= i64::MAX as u64,
                    "result revision exceeds SQLite range"
                );
            }
            for dependency in &task.dependencies {
                anyhow::ensure!(
                    dependency.required_result_revision.get() <= i64::MAX as u64,
                    "dependency revision exceeds SQLite range"
                );
            }
        }
        for goal in &self.goals {
            let goal_tasks: Vec<Task> = self
                .tasks
                .iter()
                .filter(|task| task.goal_id == goal.id)
                .cloned()
                .collect();
            if !goal_tasks.is_empty() {
                WorkPlan {
                    workspace_id: workspace_id.clone(),
                    goal_id: goal.id.clone(),
                    tasks: goal_tasks,
                }
                .validate()?;
            }
        }
        let mut attempts = HashMap::new();
        let mut numbers = HashSet::new();
        let mut operations = HashSet::new();
        let mut configs = HashMap::new();
        configs.insert(self.config.revision, &self.config);
        for attempt in &self.attempts {
            attempt.validate()?;
            anyhow::ensure!(
                &attempt.workspace_id == workspace_id
                    && tasks.contains_key(&attempt.task_id)
                    && attempts.insert(&attempt.id, attempt).is_none()
                    && numbers.insert((&attempt.task_id, attempt.number))
                    && operations.insert(&attempt.operation_id),
                "attempt ownership or operation identity mismatch"
            );
            anyhow::ensure!(
                attempt.config.revision <= self.config.revision,
                "attempt references future configuration"
            );
            if let Some(config) = configs.insert(attempt.config.revision, &attempt.config) {
                anyhow::ensure!(
                    config == &attempt.config,
                    "configuration revision has multiple snapshots"
                );
            }
        }
        for task in &self.tasks {
            if let Some(attempt_id) = &task.current_attempt_id {
                anyhow::ensure!(
                    attempts
                        .get(attempt_id)
                        .is_some_and(|attempt| attempt.task_id == task.id),
                    "current attempt belongs to a different task"
                );
            }
        }
        let mut events = HashSet::new();
        for event in &self.events {
            valid_id(&event.id.0)?;
            anyhow::ensure!(
                events.insert(&event.id) && event.display_payload.len() <= WORK_EVENT_MAX_BYTES,
                "event identity or payload limit exceeded"
            );
            if let Some(task_id) = &event.task_id {
                anyhow::ensure!(tasks.contains_key(task_id), "event references missing task");
            }
            if let Some(attempt_id) = &event.attempt_id {
                anyhow::ensure!(
                    attempts
                        .get(attempt_id)
                        .is_some_and(|attempt| event.task_id.as_ref() == Some(&attempt.task_id)),
                    "event attempt ownership mismatch"
                );
            }
        }
        let mut outbox = HashSet::new();
        for entry in &self.outbox {
            anyhow::ensure!(
                attempts
                    .get(&entry.attempt_id)
                    .is_some_and(|attempt| attempt.operation_id == entry.operation_id)
                    && outbox.insert((&entry.attempt_id, &entry.operation_id)),
                "outbox operation ownership mismatch"
            );
        }
        let mut targets = HashSet::new();
        let mut draft_bytes = 0usize;
        for draft in &self.drafts {
            anyhow::ensure!(
                !draft.target_key.trim().is_empty()
                    && draft.target_key.len() <= 256
                    && targets.insert(&draft.target_key)
                    && draft.text.len() <= WORK_DRAFT_MAX_BYTES,
                "draft identity or size limit exceeded"
            );
            draft_bytes = draft_bytes
                .checked_add(draft.text.len())
                .context("draft byte count overflow")?;
        }
        anyhow::ensure!(
            draft_bytes <= WORK_DRAFT_TOTAL_MAX_BYTES,
            "draft total byte limit exceeded"
        );
        Ok(())
    }
}

fn validate_binding(binding: &PersistentBinding) -> anyhow::Result<()> {
    match binding {
        PersistentBinding::Pane { pane_id } => valid_id(&pane_id.0),
        PersistentBinding::ProviderThread {
            provider,
            thread_id,
        } => {
            anyhow::ensure!(
                !provider.trim().is_empty()
                    && provider.len() <= 256
                    && !thread_id.trim().is_empty()
                    && thread_id.len() <= 8192,
                "invalid persistent main binding"
            );
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProject {
    pub revision: ProjectRevision,
    pub snapshot: CommandCenterSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved { revision: ProjectRevision },
    Conflict { actual: Option<ProjectRevision> },
}

pub struct CommandCenterStore {
    conn: Connection,
}

impl CommandCenterStore {
    /// The host calls this only when Command Center is enabled, from its persistence worker.
    pub fn open(data_directory: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_directory).context("create Command Center data directory")?;
        // macOS temp/data paths may contain a directory symlink; resolve the directory while
        // leaving the sidecar itself protected by SQLite's NOFOLLOW flag.
        let path = std::fs::canonicalize(data_directory)?.join(COMMAND_CENTER_FILE_NAME);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create Command Center sidecar"),
        }
        let mut conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let application_id: i64 = tx.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let tables: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if application_id == 0 && version == 0 && tables == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch(REMOTE_SCHEMA)?;
            tx.execute_batch(DEVICE_SCHEMA)?;
            tx.execute(
                "UPDATE work_store_meta SET schema_digest=?1,source_digest=?2 WHERE id=1",
                rusqlite::params![schema_digest(&tx)?, source_digest(SCHEMA_VERSION)],
            )?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else {
            anyhow::ensure!(
                application_id == APPLICATION_ID,
                "unrecognized Command Center sidecar"
            );
            anyhow::ensure!(
                (1..=SCHEMA_VERSION).contains(&version),
                "unsupported Command Center sidecar version"
            );
            let format: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT format,schema_digest,source_digest FROM work_store_meta WHERE id=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((format, digest, source)) = format else {
                anyhow::bail!("missing Command Center sidecar format");
            };
            anyhow::ensure!(
                format == "deppy-command-center-v2"
                    && source == source_digest(version)
                    && digest == schema_digest(&tx)?,
                "invalid Command Center sidecar schema"
            );
            if version < SCHEMA_VERSION {
                if version < 2 {
                    tx.execute_batch(REMOTE_SCHEMA)?;
                }
                if version < 3 {
                    tx.execute_batch(DEVICE_SCHEMA)?;
                }
                tx.execute(
                    "UPDATE work_store_meta SET schema_digest=?1,source_digest=?2 WHERE id=1",
                    rusqlite::params![schema_digest(&tx)?, source_digest(SCHEMA_VERSION)],
                )?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
        }
        tx.commit()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self { conn })
    }

    pub fn load_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> anyhow::Result<Option<StoredProject>> {
        valid_id(&workspace_id.0)?;
        let tx = self.conn.unchecked_transaction()?;
        let project = read_project(&tx, workspace_id)?;
        tx.commit()?;
        Ok(project)
    }

    pub fn save_workspace(
        &mut self,
        snapshot: &CommandCenterSnapshot,
        expected: Option<ProjectRevision>,
    ) -> anyhow::Result<SaveOutcome> {
        snapshot.validate()?;
        let bytes = encode_snapshot(snapshot)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let workspace_id = &snapshot.config.workspace_id;
        let previous = read_project(&tx, workspace_id)?;
        let actual = previous.as_ref().map(|project| project.revision);
        if actual != expected {
            return Ok(SaveOutcome::Conflict { actual });
        }
        if let Some(previous) = &previous {
            validate_update(&previous.snapshot, snapshot)?;
        } else {
            anyhow::ensure!(
                snapshot.config.revision.get() == 1,
                "new project configuration must start at revision 1"
            );
        }
        let revision = ProjectRevision::new(
            actual
                .map_or(Some(1), |revision| revision.get().checked_add(1))
                .context("project revision overflow")?,
        )?;
        // The snapshot and its relational ownership index are one transaction. Replacing the
        // index cannot expose a half-written task/outbox or erase the previous draft on failure.
        tx.execute(
            "DELETE FROM work_project_configs WHERE workspace_id=?1",
            [&workspace_id.0],
        )?;
        tx.execute("INSERT INTO work_project_configs(workspace_id,revision,config_revision,snapshot) VALUES(?1,?2,?3,?4)", rusqlite::params![workspace_id.0, revision.get() as i64, snapshot.config.revision.get() as i64, bytes])?;
        for goal in &snapshot.goals {
            tx.execute(
                "INSERT INTO work_goals(workspace_id,goal_id,submission_id) VALUES(?1,?2,?3)",
                rusqlite::params![workspace_id.0, goal.id.0, goal.submission_id.0],
            )?;
        }
        for task in &snapshot.tasks {
            tx.execute("INSERT INTO work_tasks(workspace_id,task_id,goal_id,parent_task_id,current_attempt_id) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![workspace_id.0, task.id.0, task.goal_id.0, task.parent_task_id.as_ref().map(|id| &id.0), task.current_attempt_id.as_ref().map(|id| &id.0)])?;
        }
        for attempt in &snapshot.attempts {
            tx.execute("INSERT INTO work_attempts(workspace_id,attempt_id,task_id,number,operation_id) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![workspace_id.0, attempt.id.0, attempt.task_id.0, attempt.number, attempt.operation_id.0])?;
        }
        for task in &snapshot.tasks {
            for dependency in &task.dependencies {
                tx.execute("INSERT INTO work_dependencies(workspace_id,task_id,required_task_id,required_result_revision) VALUES(?1,?2,?3,?4)", rusqlite::params![workspace_id.0, task.id.0, dependency.task_id.0, dependency.required_result_revision.get() as i64])?;
            }
        }
        for event in &snapshot.events {
            tx.execute("INSERT INTO work_events(workspace_id,event_id,task_id,attempt_id) VALUES(?1,?2,?3,?4)", rusqlite::params![workspace_id.0, event.id.0, event.task_id.as_ref().map(|id| &id.0), event.attempt_id.as_ref().map(|id| &id.0)])?;
        }
        for entry in &snapshot.outbox {
            let state = match entry.state {
                DispatchOutboxState::Pending => "pending",
                DispatchOutboxState::Admitted => "admitted",
                DispatchOutboxState::Acknowledged => "acknowledged",
                DispatchOutboxState::Uncertain => "uncertain",
            };
            tx.execute("INSERT INTO work_dispatch_outbox(workspace_id,attempt_id,operation_id,state) VALUES(?1,?2,?3,?4)", rusqlite::params![workspace_id.0, entry.attempt_id.0, entry.operation_id.0, state])?;
        }
        tx.commit()
            .context("commit Command Center project snapshot")?;
        Ok(SaveOutcome::Saved { revision })
    }
}

fn read_project(
    conn: &Connection,
    workspace_id: &WorkspaceId,
) -> anyhow::Result<Option<StoredProject>> {
    let metadata: Option<(i64, i64, i64)> = conn.query_row("SELECT revision,config_revision,length(snapshot) FROM work_project_configs WHERE workspace_id=?1", [&workspace_id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((revision, config_revision, byte_count)) = metadata else {
        return Ok(None);
    };
    anyhow::ensure!(
        (0..=WORK_PROJECT_SNAPSHOT_MAX_BYTES as i64).contains(&byte_count),
        "persisted snapshot byte limit exceeded"
    );
    let bytes: Vec<u8> = conn.query_row(
        "SELECT snapshot FROM work_project_configs WHERE workspace_id=?1",
        [&workspace_id.0],
        |row| row.get(0),
    )?;
    let snapshot: CommandCenterSnapshot =
        serde_json::from_slice(&bytes).context("decode Command Center snapshot")?;
    snapshot.validate()?;
    anyhow::ensure!(
        &snapshot.config.workspace_id == workspace_id
            && i64::try_from(snapshot.config.revision.get())? == config_revision,
        "persisted project owner or configuration revision mismatch"
    );
    Ok(Some(StoredProject {
        revision: ProjectRevision::new(u64::try_from(revision)?)?,
        snapshot,
    }))
}

fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn schema_digest(conn: &Connection) -> anyhow::Result<String> {
    let mut query = conn.prepare(
        "SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let mut rows = query.query([])?;
    let mut digest = Sha256::new();
    let mut bytes = 0usize;
    let mut count = 0usize;
    while let Some(row) = rows.next()? {
        count += 1;
        anyhow::ensure!(count <= 16, "unexpected Command Center schema entry count");
        for column in 0..3 {
            let value: String = row.get(column)?;
            bytes = bytes
                .checked_add(value.len())
                .context("schema byte count overflow")?;
            anyhow::ensure!(
                bytes <= 64 * 1024,
                "Command Center schema byte limit exceeded"
            );
            digest.update(value.as_bytes());
            digest.update([0]);
        }
    }
    Ok(digest_hex(&digest.finalize()))
}

fn encode_snapshot(snapshot: &CommandCenterSnapshot) -> anyhow::Result<Vec<u8>> {
    struct BoundedWriter(Vec<u8>);
    impl std::io::Write for BoundedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .0
                .len()
                .checked_add(bytes.len())
                .is_none_or(|length| length > WORK_PROJECT_SNAPSHOT_MAX_BYTES)
            {
                return Err(std::io::Error::other(
                    "project snapshot byte limit exceeded",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut writer, snapshot)
        .context("encode bounded Command Center snapshot")?;
    Ok(writer.0)
}

// Archival is explicit: only terminal records with acknowledged dispatch receipts may disappear,
// and snapshot validation still requires every remaining parent/dependency/current-attempt FK.
fn validate_update(
    previous: &CommandCenterSnapshot,
    next: &CommandCenterSnapshot,
) -> anyhow::Result<()> {
    if previous.config != next.config {
        anyhow::ensure!(
            previous.config.revision.next() == Some(next.config.revision),
            "configuration revision conflict"
        );
    }
    let goals: HashMap<_, _> = previous.goals.iter().map(|goal| (&goal.id, goal)).collect();
    let submissions: HashMap<_, _> = previous
        .goals
        .iter()
        .map(|goal| (&goal.submission_id, &goal.id))
        .collect();
    for goal in &next.goals {
        if let Some(old) = goals.get(&goal.id) {
            anyhow::ensure!(
                old.submission_id == goal.submission_id,
                "goal submission identity is immutable"
            );
        }
        if let Some(id) = submissions.get(&goal.submission_id) {
            anyhow::ensure!(
                **id == goal.id,
                "submission already belongs to another goal"
            );
        }
    }
    let tasks: HashMap<_, _> = previous.tasks.iter().map(|task| (&task.id, task)).collect();
    for task in &next.tasks {
        if let Some(old) = tasks.get(&task.id) {
            anyhow::ensure!(
                old.goal_id == task.goal_id,
                "task goal identity is immutable"
            );
            if let Some(result) = &old.result {
                anyhow::ensure!(
                    task.result
                        .as_ref()
                        .is_some_and(|next| next.revision >= result.revision),
                    "task result revision cannot regress or disappear"
                );
            }
        }
    }
    let retained_tasks: HashSet<_> = next.tasks.iter().map(|task| &task.id).collect();
    for task in &previous.tasks {
        anyhow::ensure!(
            retained_tasks.contains(&task.id) || task.state.is_terminal(),
            "active task cannot be archived"
        );
    }
    let attempts: HashMap<_, _> = previous
        .attempts
        .iter()
        .map(|attempt| (&attempt.id, attempt))
        .collect();
    let operations: HashMap<_, _> = previous
        .attempts
        .iter()
        .map(|attempt| (&attempt.operation_id, &attempt.id))
        .collect();
    for attempt in &next.attempts {
        if let Some(old) = attempts.get(&attempt.id) {
            anyhow::ensure!(
                old.task_id == attempt.task_id
                    && old.number == attempt.number
                    && old.operation_id == attempt.operation_id
                    && old.config == attempt.config
                    && old.folder == attempt.folder,
                "attempt dispatch identity and settings are immutable"
            );
            anyhow::ensure!(
                attempt.last_event_sequence >= old.last_event_sequence,
                "attempt event sequence cannot regress"
            );
            if old.runtime_binding.is_some() {
                anyhow::ensure!(
                    old.runtime_binding == attempt.runtime_binding,
                    "attempt runtime binding is immutable"
                );
            }
            if old.persistent_binding.is_some() {
                anyhow::ensure!(
                    old.persistent_binding == attempt.persistent_binding,
                    "attempt persistent binding is immutable"
                );
            }
        }
        if let Some(id) = operations.get(&attempt.operation_id) {
            anyhow::ensure!(
                **id == attempt.id,
                "operation already belongs to another attempt"
            );
        }
    }
    let retained_attempts: HashSet<_> = next.attempts.iter().map(|attempt| &attempt.id).collect();
    for attempt in &previous.attempts {
        anyhow::ensure!(
            retained_attempts.contains(&attempt.id) || attempt.state.is_terminal(),
            "active attempt cannot be archived"
        );
    }
    let outbox: HashMap<_, _> = next
        .outbox
        .iter()
        .map(|entry| ((&entry.attempt_id, &entry.operation_id), entry.state))
        .collect();
    for entry in &previous.outbox {
        if retained_attempts.contains(&entry.attempt_id) {
            let state = outbox
                .get(&(&entry.attempt_id, &entry.operation_id))
                .context("retained attempt cannot lose its outbox receipt")?;
            let valid = match entry.state {
                DispatchOutboxState::Pending => true,
                DispatchOutboxState::Acknowledged => *state == DispatchOutboxState::Acknowledged,
                DispatchOutboxState::Admitted | DispatchOutboxState::Uncertain => {
                    *state != DispatchOutboxState::Pending
                }
            };
            anyhow::ensure!(
                valid,
                "outbox receipt cannot regress to a retransmittable state"
            );
        } else {
            anyhow::ensure!(
                entry.state == DispatchOutboxState::Acknowledged,
                "unacknowledged dispatch receipt cannot be archived"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "work_management_tests.rs"]
mod tests;
