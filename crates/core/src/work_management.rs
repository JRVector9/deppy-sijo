//! Bounded work identities and records shared by persistence and host projections.

use crate::{MuxPaneId, SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

pub const WORK_INPUT_MAX_BYTES: usize = 16 * 1024;
pub const WORK_DRAFT_MAX_BYTES: usize = 1024 * 1024;
pub const WORK_DRAFT_TOTAL_MAX_BYTES: usize = 32 * 1024 * 1024;
pub const WORK_DRAFT_MAX_COUNT: usize = 256;
pub const WORK_PLAN_MAX_TASKS: usize = 64;
pub const WORK_EVENT_MAX_BYTES: usize = 8 * 1024;
pub const WORK_PAGE_MAX_ROWS: usize = 100;
pub const WORK_TITLE_MAX_BYTES: usize = 256;
pub const WORK_FOLDER_MAX_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}

macro_rules! work_id {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(#[serde(deserialize_with = "deserialize_id")] pub String);

        impl $name {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self { Self(uuid::Uuid::new_v4().to_string()) }
            pub fn from_string(value: String) -> Result<Self, ValidationError> {
                validate_id(&value)?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl std::str::FromStr for $name {
            type Err = ValidationError;
            fn from_str(value: &str) -> Result<Self, Self::Err> { Self::from_string(value.into()) }
        }
    )+};
}

work_id!(
    GoalId,
    TaskId,
    AttemptId,
    SubmissionId,
    OperationId,
    EventId,
    RuntimeIncarnationId
);

macro_rules! revision {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);
        impl $name {
            pub fn new(value: u64) -> Result<Self, ValidationError> {
                if value == 0 { return Err(ValidationError("revision must be nonzero".into())); }
                Ok(Self(value))
            }
            pub fn get(self) -> u64 { self.0 }
            pub fn next(self) -> Option<Self> { self.0.checked_add(1).map(Self) }
        }
    )+};
}

revision!(ConfigRevision, ResultRevision);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSelection {
    #[serde(deserialize_with = "deserialize_title")]
    pub provider: String,
    #[serde(deserialize_with = "deserialize_optional_title")]
    pub model: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_title")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub workspace_id: WorkspaceId,
    pub revision: ConfigRevision,
    #[serde(deserialize_with = "deserialize_title")]
    pub name: String,
    #[serde(deserialize_with = "deserialize_folder")]
    pub folder: String,
    pub main_agent: AgentSelection,
    pub worker_agent: AgentSelection,
    pub max_workers: u8,
    pub confirm_plan: bool,
    pub use_worktrees: bool,
}

/// These are workflow facts, separate from provider visual/activity states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    PlanReview,
    Queued,
    Running,
    WaitingApproval,
    WaitingInput,
    ResultSubmitted,
    VerificationFailed,
    AwaitingUserConfirmation,
    Completed,
    Failed,
    Cancelled,
    ObservationUnknown,
}

impl WorkState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub workspace_id: WorkspaceId,
    pub id: GoalId,
    pub submission_id: SubmissionId,
    #[serde(deserialize_with = "deserialize_title")]
    pub title: String,
    #[serde(deserialize_with = "deserialize_input")]
    pub prompt: String,
    pub state: WorkState,
}

/// Restart-safe navigation identity. Never contains a process-local SessionId.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistentBinding {
    Pane {
        pane_id: MuxPaneId,
    },
    ProviderThread {
        #[serde(deserialize_with = "deserialize_title")]
        provider: String,
        #[serde(deserialize_with = "deserialize_folder")]
        thread_id: String,
    },
}

/// A host must allocate a fresh incarnation for every runtime lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeBinding {
    pub incarnation: RuntimeIncarnationId,
    pub session_id: SessionId,
    pub execution_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    NotRun,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub revision: ResultRevision,
    #[serde(deserialize_with = "deserialize_event_payload")]
    pub summary: String,
    #[serde(deserialize_with = "deserialize_optional_title")]
    pub source_revision: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_title")]
    pub artifact_revision: Option<String>,
    pub verification: VerificationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDependency {
    pub task_id: TaskId,
    pub required_result_revision: ResultRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub workspace_id: WorkspaceId,
    pub id: TaskId,
    pub goal_id: GoalId,
    pub parent_task_id: Option<TaskId>,
    #[serde(deserialize_with = "deserialize_title")]
    pub title: String,
    #[serde(deserialize_with = "deserialize_input")]
    pub instructions: String,
    #[serde(deserialize_with = "deserialize_event_payload")]
    pub acceptance_criteria: String,
    pub state: WorkState,
    pub current_attempt_id: Option<AttemptId>,
    #[serde(deserialize_with = "deserialize_dependencies")]
    pub dependencies: Vec<TaskDependency>,
    pub result: Option<TaskResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub workspace_id: WorkspaceId,
    pub id: AttemptId,
    pub task_id: TaskId,
    pub number: u32,
    pub operation_id: OperationId,
    /// Immutable settings captured at dispatch, not a reference to latest config.
    pub config: ProjectConfig,
    #[serde(deserialize_with = "deserialize_folder")]
    pub folder: String,
    pub persistent_binding: Option<PersistentBinding>,
    pub runtime_binding: Option<RuntimeBinding>,
    pub state: WorkState,
    /// Sequence is monotonically increasing within this attempt, starting at 1.
    pub last_event_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPlan {
    pub workspace_id: WorkspaceId,
    pub goal_id: GoalId,
    #[serde(deserialize_with = "deserialize_tasks")]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkEvent {
    pub id: EventId,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub runtime_binding: RuntimeBinding,
    pub sequence: u64,
    pub kind: WorkEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkEventKind {
    InputAdmitted {
        operation_id: OperationId,
    },
    Started,
    WaitingApproval,
    WaitingInput,
    ResultSubmitted {
        result: TaskResult,
    },
    VerificationFinished {
        result_revision: ResultRevision,
        outcome: VerificationState,
    },
    UserConfirmed {
        result_revision: ResultRevision,
    },
    Failed {
        #[serde(deserialize_with = "deserialize_event_payload")]
        message: String,
    },
    Cancelled,
    ObservationLost,
    /// Process exit alone is not evidence that a result was verified or accepted.
    ProcessExited,
}

fn validate_id(value: &str) -> Result<(), ValidationError> {
    bounded("id", value, 128, false)
}

fn bounded(field: &str, value: &str, max: usize, allow_empty: bool) -> Result<(), ValidationError> {
    if value.len() > max || (!allow_empty && value.trim().is_empty()) {
        Err(ValidationError(format!(
            "{field} must contain {}..={max} UTF-8 bytes",
            usize::from(!allow_empty)
        )))
    } else {
        Ok(())
    }
}

macro_rules! string_deserializer {
    ($name:ident, $limit:expr, $empty:expr) => {
        fn $name<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
            let value = String::deserialize(deserializer)?;
            bounded(stringify!($name), &value, $limit, $empty).map_err(serde::de::Error::custom)?;
            Ok(value)
        }
    };
}

string_deserializer!(deserialize_id, 128, false);
string_deserializer!(deserialize_title, WORK_TITLE_MAX_BYTES, false);
string_deserializer!(deserialize_folder, WORK_FOLDER_MAX_BYTES, false);
string_deserializer!(deserialize_input, WORK_INPUT_MAX_BYTES, false);
string_deserializer!(deserialize_event_payload, WORK_EVENT_MAX_BYTES, true);

fn deserialize_optional_title<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        bounded("optional title", value, WORK_TITLE_MAX_BYTES, false)
            .map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_bounded_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Bounded<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {WORK_PLAN_MAX_TASKS} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element()? {
                if values.len() == WORK_PLAN_MAX_TASKS {
                    return Err(serde::de::Error::custom("plan entry limit exceeded"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded(std::marker::PhantomData))
}

fn deserialize_tasks<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Task>, D::Error> {
    deserialize_bounded_vec(d)
}
fn deserialize_dependencies<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<TaskDependency>, D::Error> {
    deserialize_bounded_vec(d)
}

impl AgentSelection {
    pub fn validate(&self) -> Result<(), ValidationError> {
        bounded("provider", &self.provider, WORK_TITLE_MAX_BYTES, false)?;
        for text in [&self.model, &self.effort].into_iter().flatten() {
            bounded("model/effort", text, WORK_TITLE_MAX_BYTES, false)?;
        }
        Ok(())
    }
}

impl ProjectConfig {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_id(&self.workspace_id.0)?;
        ConfigRevision::new(self.revision.0)?;
        bounded("name", &self.name, WORK_TITLE_MAX_BYTES, false)?;
        bounded("folder", &self.folder, WORK_FOLDER_MAX_BYTES, false)?;
        self.main_agent.validate()?;
        self.worker_agent.validate()?;
        if !(1..=8).contains(&self.max_workers) {
            return Err(ValidationError("max_workers must be 1..=8".into()));
        }
        Ok(())
    }
}

impl Goal {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_id(&self.workspace_id.0)?;
        validate_id(&self.id.0)?;
        validate_id(&self.submission_id.0)?;
        bounded("title", &self.title, WORK_TITLE_MAX_BYTES, false)?;
        validate_input(&self.prompt)
    }
}

impl TaskResult {
    pub fn validate(&self) -> Result<(), ValidationError> {
        ResultRevision::new(self.revision.0)?;
        bounded("summary", &self.summary, WORK_EVENT_MAX_BYTES, true)?;
        for text in [&self.source_revision, &self.artifact_revision]
            .into_iter()
            .flatten()
        {
            bounded("revision", text, WORK_TITLE_MAX_BYTES, false)?;
        }
        Ok(())
    }
}

impl Task {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_id(&self.workspace_id.0)?;
        validate_id(&self.id.0)?;
        validate_id(&self.goal_id.0)?;
        if let Some(id) = &self.parent_task_id {
            validate_id(&id.0)?;
        }
        if let Some(id) = &self.current_attempt_id {
            validate_id(&id.0)?;
        }
        bounded("title", &self.title, WORK_TITLE_MAX_BYTES, false)?;
        validate_input(&self.instructions)?;
        bounded(
            "acceptance criteria",
            &self.acceptance_criteria,
            WORK_EVENT_MAX_BYTES,
            true,
        )?;
        if self.dependencies.len() > WORK_PLAN_MAX_TASKS {
            return Err(ValidationError("dependency limit exceeded".into()));
        }
        let mut dependencies = HashSet::new();
        for dependency in &self.dependencies {
            validate_id(&dependency.task_id.0)?;
            ResultRevision::new(dependency.required_result_revision.0)?;
            if dependency.task_id == self.id || !dependencies.insert(&dependency.task_id) {
                return Err(ValidationError("self/duplicate dependency".into()));
            }
        }
        if let Some(result) = &self.result {
            result.validate()?;
        }
        Ok(())
    }
}

impl RuntimeBinding {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_id(&self.incarnation.0)?;
        if self.session_id.0 == 0 || self.execution_generation == 0 {
            return Err(ValidationError(
                "runtime session and execution generation must be nonzero".into(),
            ));
        }
        Ok(())
    }
}

impl Attempt {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_id(&self.workspace_id.0)?;
        validate_id(&self.id.0)?;
        validate_id(&self.task_id.0)?;
        validate_id(&self.operation_id.0)?;
        self.config.validate()?;
        if self.config.workspace_id != self.workspace_id || self.number == 0 {
            return Err(ValidationError("attempt owner/number mismatch".into()));
        }
        bounded("attempt folder", &self.folder, WORK_FOLDER_MAX_BYTES, false)?;
        if let Some(binding) = &self.runtime_binding {
            binding.validate()?;
        }
        if let Some(binding) = &self.persistent_binding {
            match binding {
                PersistentBinding::Pane { pane_id } => validate_id(&pane_id.0)?,
                PersistentBinding::ProviderThread {
                    provider,
                    thread_id,
                } => {
                    bounded("provider", provider, WORK_TITLE_MAX_BYTES, false)?;
                    bounded("thread", thread_id, WORK_FOLDER_MAX_BYTES, false)?;
                }
            }
        }
        Ok(())
    }
}

impl WorkPlan {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.tasks.is_empty() || self.tasks.len() > WORK_PLAN_MAX_TASKS {
            return Err(ValidationError("plan must contain 1..=64 tasks".into()));
        }
        validate_id(&self.workspace_id.0)?;
        validate_id(&self.goal_id.0)?;
        let mut tasks = HashMap::new();
        for task in &self.tasks {
            task.validate()?;
            if task.workspace_id != self.workspace_id
                || task.goal_id != self.goal_id
                || tasks.insert(&task.id, task).is_some()
            {
                return Err(ValidationError("plan task owner/identity mismatch".into()));
            }
        }
        for task in &self.tasks {
            let edges = task
                .dependencies
                .iter()
                .map(|d| &d.task_id)
                .chain(task.parent_task_id.iter());
            for id in edges {
                if !tasks.contains_key(id) {
                    return Err(ValidationError("plan references missing task".into()));
                }
            }
        }
        fn visit<'a>(
            id: &'a TaskId,
            tasks: &HashMap<&'a TaskId, &'a Task>,
            visiting: &mut HashSet<&'a TaskId>,
            done: &mut HashSet<&'a TaskId>,
        ) -> bool {
            if done.contains(id) {
                return true;
            }
            if !visiting.insert(id) {
                return false;
            }
            let task = tasks[id];
            for edge in task
                .dependencies
                .iter()
                .map(|d| &d.task_id)
                .chain(task.parent_task_id.iter())
            {
                if !visit(edge, tasks, visiting, done) {
                    return false;
                }
            }
            visiting.remove(id);
            done.insert(id);
            true
        }
        let mut done = HashSet::new();
        for id in tasks.keys() {
            if !visit(id, &tasks, &mut HashSet::new(), &mut done) {
                return Err(ValidationError(
                    "plan contains a dependency/parent cycle".into(),
                ));
            }
        }
        Ok(())
    }
}

impl WorkEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        for id in [
            &self.id.0,
            &self.workspace_id.0,
            &self.task_id.0,
            &self.attempt_id.0,
        ] {
            validate_id(id)?;
        }
        self.runtime_binding.validate()?;
        if self.sequence == 0 {
            return Err(ValidationError("event sequence must be nonzero".into()));
        }
        match &self.kind {
            WorkEventKind::InputAdmitted { operation_id } => validate_id(&operation_id.0)?,
            WorkEventKind::ResultSubmitted { result } => result.validate()?,
            WorkEventKind::VerificationFinished {
                result_revision, ..
            }
            | WorkEventKind::UserConfirmed { result_revision } => {
                ResultRevision::new(result_revision.0)?;
            }
            WorkEventKind::Failed { message } => {
                bounded("event payload", message, WORK_EVENT_MAX_BYTES, true)?
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn validate_input(text: &str) -> Result<(), ValidationError> {
    bounded("input", text, WORK_INPUT_MAX_BYTES, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_management_input_rejects_utf8_bytes_without_mutation() {
        let draft = "가".repeat(WORK_INPUT_MAX_BYTES / 3 + 1);
        let original = draft.clone();
        assert!(validate_input(&draft).is_err());
        assert_eq!(draft, original);
        assert!(validate_input(&"x".repeat(WORK_INPUT_MAX_BYTES)).is_ok());
    }
}
