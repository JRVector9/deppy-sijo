pub use deppy_core::{MuxPaneId, MuxTabId, SessionId};
pub use mux::SplitDirection;

pub(crate) const RUNTIME_SESSION_CAP: usize = 256;
pub(crate) const RUNTIME_COMMAND_QUEUE_BYTES_MAX: usize = 8 * 1024 * 1024;
const TERMINAL_DIMENSION_MAX: u16 = 500;
const TERMINAL_CELL_COUNT_MAX: u32 = 65_536;
use terminal::policy::SCROLLBACK_LINES_MAX;
const COMMAND_BYTES_MAX: usize = 32 * 1024;
const ARG_ITEMS_MAX: usize = 256;
const ARG_BYTES_MAX: usize = 32 * 1024;
const ARG_AGGREGATE_BYTES_MAX: usize = 1024 * 1024;
const ENV_ITEMS_MAX: usize = 256;
const ENV_KEY_BYTES_MAX: usize = 1024;
const ENV_VALUE_BYTES_MAX: usize = 32 * 1024;
const ENV_AGGREGATE_BYTES_MAX: usize = 1024 * 1024;
/// status detector regex 상한 — **persist가 소유한다.** 복원 쪽 상한과 반드시 같아야
/// 하는데(다르면 정상 저장된 행이 손상 취급된다) 예전엔 양쪽에 같은 숫자를 따로 적고
/// 주석으로만 묶어뒀다. 한쪽만 바뀌면 조용히 어긋나므로 한 값을 공유한다.
use persist::REGEX_BYTES_MAX;
const REGEX_AGGREGATE_BYTES_MAX: usize = 256 * 1024;
const PATH_BYTES_MAX: usize = 4 * 1024;
const PANE_TITLE_BYTES_MAX: usize = 4 * 1024;
const MUX_ID_BYTES_MAX: usize = 1024;
const SPLIT_PATH_ITEMS_MAX: usize = 64;
const SEARCH_QUERY_BYTES_MAX: usize = 64 * 1024;
const SEED_CREDENTIAL_ITEMS_MAX: usize = 256;
const SEED_CREDENTIAL_ID_BYTES_MAX: usize = 1024;
const WRITE_INPUT_BYTES_MAX: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeAdmissionError(&'static str);

impl std::fmt::Debug for RuntimeAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("RuntimeAdmissionError")
            .field(&self.0)
            .finish()
    }
}

impl std::fmt::Display for RuntimeAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RuntimeAdmissionError {}

fn admission_error(code: &'static str) -> RuntimeAdmissionError {
    RuntimeAdmissionError(code)
}

/// Stable, payload-free failure returned by the public command-retention preflight.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCommandPreparationErrorCode {
    InvalidCommand,
    ResourceLimit,
}

impl RuntimeCommandPreparationErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCommand => "invalid_command",
            Self::ResourceLimit => "resource_limit",
        }
    }
}

impl std::fmt::Debug for RuntimeCommandPreparationErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::fmt::Display for RuntimeCommandPreparationErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for RuntimeCommandPreparationErrorCode {}

/// Actual bytes retained by a validated, capacity-canonicalized command. `Debug` deliberately
/// omits the value so diagnostics remain low-cardinality; callers use the accessor for accounting.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCommandRetention {
    retained_bytes: usize,
}

impl RuntimeCommandRetention {
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }
}

impl std::fmt::Debug for RuntimeCommandRetention {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RuntimeCommandRetention")
    }
}

/// Adds a prepared command to an App/runtime retention aggregate using the exact same 8-MiB
/// ceiling as the in-process queue. This keeps composition-root accounting free of copied limits.
pub fn checked_runtime_command_retention_total(
    retained_bytes: usize,
    additional_bytes: usize,
) -> Result<usize, RuntimeCommandPreparationErrorCode> {
    let total = retained_bytes
        .checked_add(additional_bytes)
        .ok_or(RuntimeCommandPreparationErrorCode::ResourceLimit)?;
    if total > RUNTIME_COMMAND_QUEUE_BYTES_MAX {
        return Err(RuntimeCommandPreparationErrorCode::ResourceLimit);
    }
    Ok(total)
}

/// Opaque launch correlation ids are bounded before entering the final host
/// queue. Current UUID producers fit comfortably while future adapters retain
/// room for namespaced ids without making durable event queues attacker-sized.
pub const AGENT_CONFIG_ID_MAX_BYTES: usize = 128;

pub(crate) fn agent_config_id_is_valid(id: &str) -> bool {
    bounded_identifier(id, AGENT_CONFIG_ID_MAX_BYTES)
}

fn bounded_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn bounded_nul_free(value: &str, max_bytes: usize, require_nonempty: bool) -> bool {
    value.len() <= max_bytes
        && (!require_nonempty || !value.is_empty())
        && !value.as_bytes().contains(&0)
}

fn dimensions_are_valid(cols: u16, rows: u16) -> bool {
    (1..=TERMINAL_DIMENSION_MAX).contains(&cols)
        && (1..=TERMINAL_DIMENSION_MAX).contains(&rows)
        && u32::from(cols)
            .checked_mul(u32::from(rows))
            .is_some_and(|cells| cells <= TERMINAL_CELL_COUNT_MAX)
}

fn validate_args(args: &[String]) -> Result<(), RuntimeAdmissionError> {
    if args.len() > ARG_ITEMS_MAX {
        return Err(admission_error("runtime_command_args_items_invalid"));
    }
    let mut retained_bytes = 0usize;
    for arg in args {
        if !bounded_nul_free(arg, ARG_BYTES_MAX, false) {
            return Err(admission_error("runtime_command_arg_invalid"));
        }
        retained_bytes = retained_bytes
            .checked_add(arg.len())
            .ok_or_else(|| admission_error("runtime_command_args_bytes_invalid"))?;
        if retained_bytes > ARG_AGGREGATE_BYTES_MAX {
            return Err(admission_error("runtime_command_args_bytes_invalid"));
        }
    }
    Ok(())
}

fn validate_env_key(key: &str) -> bool {
    bounded_identifier(key, ENV_KEY_BYTES_MAX) && !key.as_bytes().contains(&b'=')
}

pub(crate) fn validate_runtime_identifier(
    value: &str,
    max_bytes: usize,
) -> Result<(), RuntimeAdmissionError> {
    if !bounded_identifier(value, max_bytes) {
        return Err(admission_error("runtime_identifier_invalid"));
    }
    Ok(())
}

pub(crate) fn validate_env_entries(
    plain: &[(String, String)],
    secrets: &[(String, String)],
) -> Result<(), RuntimeAdmissionError> {
    validate_env_entries_with_base(&[], plain, secrets)
}

pub(crate) fn validate_env_entries_with_base(
    base_plain: &[(String, String)],
    plain: &[(String, String)],
    secrets: &[(String, String)],
) -> Result<(), RuntimeAdmissionError> {
    let item_count = base_plain
        .len()
        .checked_add(plain.len())
        .and_then(|items| items.checked_add(secrets.len()))
        .ok_or_else(|| admission_error("runtime_env_items_invalid"))?;
    if item_count > ENV_ITEMS_MAX {
        return Err(admission_error("runtime_env_items_invalid"));
    }
    let mut retained_bytes = 0usize;
    for (key, value) in base_plain.iter().chain(plain) {
        if !validate_env_key(key) || !bounded_nul_free(value, ENV_VALUE_BYTES_MAX, false) {
            return Err(admission_error("runtime_env_plain_invalid"));
        }
        retained_bytes = retained_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| admission_error("runtime_env_bytes_invalid"))?;
        if retained_bytes > ENV_AGGREGATE_BYTES_MAX {
            return Err(admission_error("runtime_env_bytes_invalid"));
        }
    }
    for (key, credential_id) in secrets {
        if !validate_env_key(key)
            || !bounded_identifier(credential_id, SEED_CREDENTIAL_ID_BYTES_MAX)
        {
            return Err(admission_error("runtime_env_secret_invalid"));
        }
        retained_bytes = retained_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(credential_id.len()))
            .ok_or_else(|| admission_error("runtime_env_bytes_invalid"))?;
        if retained_bytes > ENV_AGGREGATE_BYTES_MAX {
            return Err(admission_error("runtime_env_bytes_invalid"));
        }
    }
    Ok(())
}

pub(crate) fn validate_runtime_path(path: &std::path::Path) -> Result<(), RuntimeAdmissionError> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() > PATH_BYTES_MAX || bytes.contains(&0) {
        return Err(admission_error("runtime_path_invalid"));
    }
    Ok(())
}

pub(crate) fn validate_launch_spec(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&std::path::Path>,
) -> Result<(), RuntimeAdmissionError> {
    if !bounded_nul_free(program, COMMAND_BYTES_MAX, true) {
        return Err(admission_error("runtime_command_program_invalid"));
    }
    validate_args(args)?;
    validate_env_entries(env, &[])?;
    if let Some(cwd) = cwd {
        validate_runtime_path(cwd)?;
    }
    Ok(())
}

/// Launch specs reserve exactly one runtime-owned environment slot outside the external 256-item
/// admission budget. This keeps an exact-limit caller valid after `DEPPY_SESSION_ID` injection,
/// while still bounding and validating the internal key/value before process creation.
pub(crate) fn validate_launch_spec_with_internal_env(
    program: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&std::path::Path>,
    internal_key: &str,
) -> Result<(), RuntimeAdmissionError> {
    if !bounded_nul_free(program, COMMAND_BYTES_MAX, true) {
        return Err(admission_error("runtime_command_program_invalid"));
    }
    validate_args(args)?;

    let mut external_items = 0usize;
    let mut external_bytes = 0usize;
    let mut internal_items = 0usize;
    for (key, value) in env {
        if !validate_env_key(key) || !bounded_nul_free(value, ENV_VALUE_BYTES_MAX, false) {
            return Err(admission_error("runtime_env_plain_invalid"));
        }
        if key == internal_key {
            internal_items = internal_items
                .checked_add(1)
                .ok_or_else(|| admission_error("runtime_internal_env_invalid"))?;
            continue;
        }
        external_items = external_items
            .checked_add(1)
            .ok_or_else(|| admission_error("runtime_env_items_invalid"))?;
        external_bytes = external_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| admission_error("runtime_env_bytes_invalid"))?;
    }
    if internal_items != 1 || external_items > ENV_ITEMS_MAX {
        return Err(admission_error("runtime_internal_env_invalid"));
    }
    if external_bytes > ENV_AGGREGATE_BYTES_MAX {
        return Err(admission_error("runtime_env_bytes_invalid"));
    }
    if let Some(cwd) = cwd {
        validate_runtime_path(cwd)?;
    }
    Ok(())
}

fn validate_regexes(regexes: [Option<&str>; 4]) -> Result<(), RuntimeAdmissionError> {
    let mut retained_bytes = 0usize;
    for regex in regexes.into_iter().flatten() {
        if regex.len() > REGEX_BYTES_MAX {
            return Err(admission_error("runtime_command_regex_invalid"));
        }
        retained_bytes = retained_bytes
            .checked_add(regex.len())
            .ok_or_else(|| admission_error("runtime_command_regex_bytes_invalid"))?;
        if retained_bytes > REGEX_AGGREGATE_BYTES_MAX {
            return Err(admission_error("runtime_command_regex_bytes_invalid"));
        }
    }
    Ok(())
}

fn retained_add(total: &mut usize, bytes: usize) -> Result<(), RuntimeAdmissionError> {
    *total = total
        .checked_add(bytes)
        .ok_or_else(|| admission_error("runtime_command_retained_bytes_invalid"))?;
    Ok(())
}

fn retained_string(total: &mut usize, value: &String) -> Result<(), RuntimeAdmissionError> {
    retained_add(total, value.capacity())
}

fn retained_strings(total: &mut usize, values: &Vec<String>) -> Result<(), RuntimeAdmissionError> {
    retained_add(
        total,
        values
            .capacity()
            .checked_mul(std::mem::size_of::<String>())
            .ok_or_else(|| admission_error("runtime_command_retained_bytes_invalid"))?,
    )?;
    for value in values {
        retained_string(total, value)?;
    }
    Ok(())
}

fn retained_env(
    total: &mut usize,
    values: &Vec<(String, String)>,
) -> Result<(), RuntimeAdmissionError> {
    retained_add(
        total,
        values
            .capacity()
            .checked_mul(std::mem::size_of::<(String, String)>())
            .ok_or_else(|| admission_error("runtime_command_retained_bytes_invalid"))?,
    )?;
    for (key, value) in values {
        retained_string(total, key)?;
        retained_string(total, value)?;
    }
    Ok(())
}

fn retained_optional_string(
    total: &mut usize,
    value: &Option<String>,
) -> Result<(), RuntimeAdmissionError> {
    if let Some(value) = value {
        retained_string(total, value)?;
    }
    Ok(())
}

/// Actual heap retained while a command waits in the internal bounded queue. Capacity, rather
/// than length, is charged so a short value with an attacker-sized spare allocation cannot evade
/// the queue byte budget. The public/wire command remains unchanged.
pub(crate) fn runtime_command_retained_bytes(
    command: &RuntimeCommand,
) -> Result<usize, RuntimeAdmissionError> {
    let mut total = std::mem::size_of::<RuntimeCommand>();
    match command {
        RuntimeCommand::SpawnAgent {
            agent_config_id,
            command,
            args,
            env_plain,
            env_secrets,
            waiting_regex,
            approval_regex,
            error_regex,
            done_regex,
            ..
        } => {
            retained_optional_string(&mut total, agent_config_id)?;
            retained_string(&mut total, command)?;
            retained_strings(&mut total, args)?;
            retained_env(&mut total, env_plain)?;
            retained_env(&mut total, env_secrets)?;
            for regex in [waiting_regex, approval_regex, error_regex, done_regex] {
                retained_optional_string(&mut total, regex)?;
            }
        }
        RuntimeCommand::WriteInput { bytes, .. } => retained_add(&mut total, bytes.capacity())?,
        RuntimeCommand::SeedRedaction { credential_ids } => {
            retained_strings(&mut total, credential_ids)?;
        }
        RuntimeCommand::RespawnArchivedAgent { extra_args, .. } => {
            retained_strings(&mut total, extra_args)?;
        }
        RuntimeCommand::SplitPane { pane, .. }
        | RuntimeCommand::ClosePane { pane }
        | RuntimeCommand::FocusPane { pane }
        | RuntimeCommand::RestoreWorkspacePane { pane } => retained_string(&mut total, &pane.0)?,
        RuntimeCommand::CloseTab { tab } | RuntimeCommand::SelectTab { tab } => {
            retained_string(&mut total, &tab.0)?;
        }
        RuntimeCommand::ResizeSplit { tab, path, .. } => {
            retained_string(&mut total, &tab.0)?;
            retained_add(&mut total, path.capacity())?;
        }
        RuntimeCommand::RenamePane { pane, title } => {
            retained_string(&mut total, &pane.0)?;
            retained_string(&mut total, title)?;
        }
        RuntimeCommand::SetSessionDefaultEnv {
            env_plain,
            env_secrets,
        } => {
            retained_env(&mut total, env_plain)?;
            retained_env(&mut total, env_secrets)?;
        }
        RuntimeCommand::SetShellCwd(cwd) => {
            if let Some(cwd) = cwd {
                retained_add(&mut total, cwd.capacity())?;
            }
        }
        RuntimeCommand::UpdateSessionCwd { cwd, .. }
        | RuntimeCommand::SearchScrollback { query: cwd, .. } => {
            retained_string(&mut total, cwd)?;
        }
        RuntimeCommand::SpawnShell { .. }
        | RuntimeCommand::Resize { .. }
        | RuntimeCommand::Scroll { .. }
        | RuntimeCommand::KillSession { .. }
        | RuntimeCommand::RestoreWorkspace
        | RuntimeCommand::SetWorkspaceState(_)
        | RuntimeCommand::SetUserStatusOverride { .. }
        | RuntimeCommand::SetTerminalCachePolicy { .. }
        | RuntimeCommand::SetRemoteViewing { .. }
        | RuntimeCommand::ScrollToBottom { .. }
        | RuntimeCommand::ScrollToPrompt { .. }
        | RuntimeCommand::ExtractLastOutput { .. }
        | RuntimeCommand::EmergencyPersistFlush
        | RuntimeCommand::FreezeSession { .. }
        | RuntimeCommand::ResumeSession { .. }
        | RuntimeCommand::NoteTurnStart { .. }
        | RuntimeCommand::SetScrollbackLimit { .. }
        | RuntimeCommand::DurableEventBarrier { .. }
        | RuntimeCommand::InspectUnattachedSessions
        | RuntimeCommand::KillUnattachedSessions => {}
    }
    Ok(total)
}

fn canonicalize_string(value: &mut String) {
    *value = std::mem::take(value).into_boxed_str().into_string();
}

fn canonicalize_strings(values: &mut Vec<String>) {
    for value in values.iter_mut() {
        canonicalize_string(value);
    }
    *values = std::mem::take(values).into_boxed_slice().into_vec();
}

fn canonicalize_env(values: &mut Vec<(String, String)>) {
    for (key, value) in values.iter_mut() {
        canonicalize_string(key);
        canonicalize_string(value);
    }
    *values = std::mem::take(values).into_boxed_slice().into_vec();
}

fn canonicalize_optional_string(value: &mut Option<String>) {
    if let Some(value) = value {
        canonicalize_string(value);
    }
}

fn canonicalize_mux_pane_id(id: &mut MuxPaneId) {
    canonicalize_string(&mut id.0);
}

fn canonicalize_mux_tab_id(id: &mut MuxTabId) {
    canonicalize_string(&mut id.0);
}

/// Values that leave the queue and may move into worker state, persistence rows, or durable
/// events are rebuilt with length-bound backing allocations. Validation must run first; this is
/// capacity canonicalization only and does not alter command semantics or wire representation.
pub(crate) fn canonicalize_host_command(command: &mut RuntimeCommand) {
    match command {
        RuntimeCommand::SpawnAgent {
            agent_config_id,
            command,
            args,
            env_plain,
            env_secrets,
            waiting_regex,
            approval_regex,
            error_regex,
            done_regex,
            ..
        } => {
            canonicalize_optional_string(agent_config_id);
            canonicalize_string(command);
            canonicalize_strings(args);
            canonicalize_env(env_plain);
            canonicalize_env(env_secrets);
            for regex in [waiting_regex, approval_regex, error_regex, done_regex] {
                canonicalize_optional_string(regex);
            }
        }
        RuntimeCommand::WriteInput { bytes, .. } => {
            *bytes = std::mem::take(bytes).into_boxed_slice().into_vec();
        }
        RuntimeCommand::SeedRedaction { credential_ids } => {
            canonicalize_strings(credential_ids);
        }
        RuntimeCommand::RespawnArchivedAgent { extra_args, .. } => {
            canonicalize_strings(extra_args);
        }
        RuntimeCommand::SplitPane { pane, .. }
        | RuntimeCommand::ClosePane { pane }
        | RuntimeCommand::FocusPane { pane }
        | RuntimeCommand::RestoreWorkspacePane { pane } => canonicalize_mux_pane_id(pane),
        RuntimeCommand::CloseTab { tab } | RuntimeCommand::SelectTab { tab } => {
            canonicalize_mux_tab_id(tab);
        }
        RuntimeCommand::ResizeSplit { tab, path, .. } => {
            canonicalize_mux_tab_id(tab);
            *path = std::mem::take(path).into_boxed_slice().into_vec();
        }
        RuntimeCommand::RenamePane { pane, title } => {
            canonicalize_mux_pane_id(pane);
            canonicalize_string(title);
        }
        RuntimeCommand::SetSessionDefaultEnv {
            env_plain,
            env_secrets,
        } => {
            canonicalize_env(env_plain);
            canonicalize_env(env_secrets);
        }
        RuntimeCommand::SetShellCwd(cwd) => {
            if let Some(cwd) = cwd {
                *cwd = std::mem::take(cwd).into_boxed_path().into_path_buf();
            }
        }
        RuntimeCommand::UpdateSessionCwd { cwd, .. }
        | RuntimeCommand::SearchScrollback { query: cwd, .. } => canonicalize_string(cwd),
        RuntimeCommand::SpawnShell { .. }
        | RuntimeCommand::Resize { .. }
        | RuntimeCommand::Scroll { .. }
        | RuntimeCommand::KillSession { .. }
        | RuntimeCommand::RestoreWorkspace
        | RuntimeCommand::SetWorkspaceState(_)
        | RuntimeCommand::SetUserStatusOverride { .. }
        | RuntimeCommand::SetTerminalCachePolicy { .. }
        | RuntimeCommand::SetRemoteViewing { .. }
        | RuntimeCommand::ScrollToBottom { .. }
        | RuntimeCommand::ScrollToPrompt { .. }
        | RuntimeCommand::ExtractLastOutput { .. }
        | RuntimeCommand::EmergencyPersistFlush
        | RuntimeCommand::FreezeSession { .. }
        | RuntimeCommand::ResumeSession { .. }
        | RuntimeCommand::NoteTurnStart { .. }
        | RuntimeCommand::SetScrollbackLimit { .. }
        | RuntimeCommand::DurableEventBarrier { .. }
        | RuntimeCommand::InspectUnattachedSessions
        | RuntimeCommand::KillUnattachedSessions => {}
    }
}

/// Validates a host command, canonicalizes all owned backing allocations to their logical
/// lengths, and only then measures its actual retained capacity. Success leaves `command` ready
/// for bounded App/runtime retention. Failure exposes only a stable low-cardinality code.
///
/// The individual command ceiling intentionally reuses the in-process queue's byte ceiling so an
/// App-retained command can never pass this preflight but fail solely because the runtime uses a
/// smaller per-command limit.
pub fn prepare_runtime_command_for_retention(
    command: &mut RuntimeCommand,
) -> Result<RuntimeCommandRetention, RuntimeCommandPreparationErrorCode> {
    prepare_runtime_command_for_retention_internal(command).map_err(|error| match error.0 {
        "runtime_command_retained_bytes_invalid" | "runtime_command_retained_bytes_limit" => {
            RuntimeCommandPreparationErrorCode::ResourceLimit
        }
        _ => RuntimeCommandPreparationErrorCode::InvalidCommand,
    })
}

pub(crate) fn prepare_runtime_command_for_retention_internal(
    command: &mut RuntimeCommand,
) -> Result<RuntimeCommandRetention, RuntimeAdmissionError> {
    validate_host_command(command)?;
    canonicalize_host_command(command);
    let retained_bytes = runtime_command_retained_bytes(command)?;
    if retained_bytes > RUNTIME_COMMAND_QUEUE_BYTES_MAX {
        return Err(admission_error("runtime_command_retained_bytes_limit"));
    }
    Ok(RuntimeCommandRetention { retained_bytes })
}

fn mux_pane_id_is_valid(id: &MuxPaneId) -> bool {
    bounded_identifier(&id.0, MUX_ID_BYTES_MAX)
}

fn mux_tab_id_is_valid(id: &MuxTabId) -> bool {
    bounded_identifier(&id.0, MUX_ID_BYTES_MAX)
}

pub(crate) fn validate_host_command(command: &RuntimeCommand) -> Result<(), RuntimeAdmissionError> {
    match command {
        RuntimeCommand::SpawnShell {
            cols,
            rows,
            scrollback_lines,
        } => {
            if !dimensions_are_valid(*cols, *rows) || *scrollback_lines > SCROLLBACK_LINES_MAX {
                return Err(admission_error("runtime_command_spawn_shell_invalid"));
            }
        }
        RuntimeCommand::SpawnAgent {
            cols,
            rows,
            scrollback_lines,
            agent_config_id,
            command,
            args,
            env_plain,
            env_secrets,
            waiting_regex,
            approval_regex,
            error_regex,
            done_regex,
        } => {
            if !dimensions_are_valid(*cols, *rows)
                || *scrollback_lines > SCROLLBACK_LINES_MAX
                || agent_config_id
                    .as_deref()
                    .is_some_and(|id| !agent_config_id_is_valid(id))
                || !bounded_nul_free(command, COMMAND_BYTES_MAX, true)
            {
                return Err(admission_error("runtime_command_spawn_agent_invalid"));
            }
            validate_args(args)?;
            validate_env_entries(env_plain, env_secrets)?;
            validate_regexes([
                waiting_regex.as_deref(),
                approval_regex.as_deref(),
                error_regex.as_deref(),
                done_regex.as_deref(),
            ])?;
        }
        RuntimeCommand::WriteInput { bytes, .. } => {
            if bytes.len() > WRITE_INPUT_BYTES_MAX {
                return Err(admission_error("runtime_command_input_invalid"));
            }
        }
        RuntimeCommand::Resize { cols, rows, .. } => {
            if !dimensions_are_valid(*cols, *rows) {
                return Err(admission_error("runtime_command_resize_invalid"));
            }
        }
        RuntimeCommand::SeedRedaction { credential_ids } => {
            if credential_ids.len() > SEED_CREDENTIAL_ITEMS_MAX
                || credential_ids
                    .iter()
                    .any(|id| !bounded_identifier(id, SEED_CREDENTIAL_ID_BYTES_MAX))
            {
                return Err(admission_error("runtime_command_seed_invalid"));
            }
        }
        RuntimeCommand::SplitPane {
            pane,
            scrollback_lines,
            ..
        } => {
            if !mux_pane_id_is_valid(pane) || *scrollback_lines > SCROLLBACK_LINES_MAX {
                return Err(admission_error("runtime_command_split_invalid"));
            }
        }
        RuntimeCommand::ResizeSplit {
            tab, path, ratio, ..
        } => {
            if !mux_tab_id_is_valid(tab)
                || path.len() > SPLIT_PATH_ITEMS_MAX
                || !ratio.is_finite()
                || !(0.0..=1.0).contains(ratio)
            {
                return Err(admission_error("runtime_command_resize_split_invalid"));
            }
        }
        RuntimeCommand::RenamePane { pane, title } => {
            if !mux_pane_id_is_valid(pane) || !bounded_nul_free(title, PANE_TITLE_BYTES_MAX, false)
            {
                return Err(admission_error("runtime_command_pane_title_invalid"));
            }
        }
        RuntimeCommand::ClosePane { pane }
        | RuntimeCommand::FocusPane { pane }
        | RuntimeCommand::RestoreWorkspacePane { pane } => {
            if !mux_pane_id_is_valid(pane) {
                return Err(admission_error("runtime_command_pane_id_invalid"));
            }
        }
        RuntimeCommand::CloseTab { tab } | RuntimeCommand::SelectTab { tab } => {
            if !mux_tab_id_is_valid(tab) {
                return Err(admission_error("runtime_command_tab_id_invalid"));
            }
        }
        RuntimeCommand::SetSessionDefaultEnv {
            env_plain,
            env_secrets,
        } => validate_env_entries(env_plain, env_secrets)?,
        RuntimeCommand::SetShellCwd(Some(cwd)) => validate_runtime_path(cwd)?,
        RuntimeCommand::UpdateSessionCwd { cwd, .. } => {
            if !bounded_nul_free(cwd, PATH_BYTES_MAX, true) {
                return Err(admission_error("runtime_path_invalid"));
            }
        }
        RuntimeCommand::SearchScrollback { query, .. } => {
            if query.len() > SEARCH_QUERY_BYTES_MAX {
                return Err(admission_error("runtime_command_search_invalid"));
            }
        }
        RuntimeCommand::SetScrollbackLimit {
            generation,
            requested,
        } => {
            if *generation == 0
                || !(terminal::policy::SCROLLBACK_SETTING_MIN..=SCROLLBACK_LINES_MAX as u32)
                    .contains(requested)
            {
                return Err(admission_error("runtime_scrollback_policy_invalid"));
            }
        }
        RuntimeCommand::DurableEventBarrier { correlation_id } => {
            if *correlation_id == 0 {
                return Err(admission_error("runtime_durable_event_barrier_invalid"));
            }
        }
        RuntimeCommand::RespawnArchivedAgent {
            cols,
            rows,
            scrollback_lines,
            extra_args,
            ..
        } => {
            if !dimensions_are_valid(*cols, *rows) || *scrollback_lines > SCROLLBACK_LINES_MAX {
                return Err(admission_error("runtime_command_respawn_archived_invalid"));
            }
            validate_args(extra_args)?;
        }
        RuntimeCommand::Scroll { .. }
        | RuntimeCommand::KillSession { .. }
        | RuntimeCommand::RestoreWorkspace
        | RuntimeCommand::SetWorkspaceState(_)
        | RuntimeCommand::SetUserStatusOverride { .. }
        | RuntimeCommand::SetShellCwd(None)
        | RuntimeCommand::SetTerminalCachePolicy { .. }
        | RuntimeCommand::SetRemoteViewing { .. }
        | RuntimeCommand::ScrollToBottom { .. }
        | RuntimeCommand::ScrollToPrompt { .. }
        | RuntimeCommand::ExtractLastOutput { .. }
        | RuntimeCommand::EmergencyPersistFlush
        | RuntimeCommand::FreezeSession { .. }
        | RuntimeCommand::ResumeSession { .. }
        | RuntimeCommand::NoteTurnStart { .. }
        | RuntimeCommand::InspectUnattachedSessions
        | RuntimeCommand::KillUnattachedSessions => {}
    }
    Ok(())
}

/// workspace 런타임 상태 (설계문서 §14.1). 현재 단일 workspace 앱에서 실효 있는 전이는
/// Active↔Warm(앱 최소화/가림 시 render/snapshot 중단, 세션은 유지). Suspended/Closed는
/// workspace "닫기"(세션 종료)가 전제라 multi-workspace 관리 도입 시 완성된다 —
/// worker는 Active가 아니면 snapshot 생성만 멈춘다(Warm 수준). 세션 kill은 안 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkspaceRuntimeState {
    /// visible pane render + snapshot (§14.4/14.3은 이 안에서 이미 visible-only)
    Active,
    /// status/log tail만 — renderer/snapshot 금지, 세션(PTY)은 유지
    Warm,
    /// layout/session metadata만 — (workspace-close 전제, 현재 미도달)
    Suspended,
    /// DB metadata만 — (workspace-close 전제, 현재 미도달)
    Closed,
}

/// UI → Runtime 명령 (설계문서 2.1). v0은 단일 셸 세션에 필요한 것만.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RuntimeCommand {
    SpawnShell {
        cols: u16,
        rows: u16,
        /// spawn 시점의 설정값 — 설정 변경이 다음 세션부터 반영되게 한다
        scrollback_lines: usize,
    },
    /// agent command 실행 (설계문서 PR-09). secret env는 credential_id 참조로
    /// 전달되고 worker가 spawn 직전에만 resolve한다 (6.3) — 값은 이 명령에 없다.
    SpawnAgent {
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        /// agent_configs.id — 세션 영속(§11.1 sessions.agent_id)에 기록된다
        agent_config_id: Option<String>,
        command: String,
        args: Vec<String>,
        env_plain: Vec<(String, String)>,
        /// (env key, credential_id)
        env_secrets: Vec<(String, String)>,
        /// status detector regex (agent_configs *_regex — PR-12)
        waiting_regex: Option<String>,
        approval_regex: Option<String>,
        error_regex: Option<String>,
        done_regex: Option<String>,
    },
    WriteInput {
        session: SessionId,
        bytes: Vec<u8>,
    },
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// scrollback 스크롤 (양수 = 과거로)
    Scroll {
        session: SessionId,
        delta: i32,
    },
    KillSession {
        session: SessionId,
    },
    /// 저장된 credential들을 로그 redaction 패턴으로 등록한다 (설계문서 7장).
    /// 값은 worker가 keyring에서 읽는다 — 명령에는 id만 실린다.
    SeedRedaction {
        credential_ids: Vec<String>,
    },
    /// focused pane을 분할하고 새 셸 세션을 attach한다 (PR-10)
    SplitPane {
        pane: MuxPaneId,
        direction: SplitDirection,
        scrollback_lines: usize,
    },
    /// pane을 닫는다 — 세션 kill 포함. 마지막 pane이면 tab도 닫힌다
    ClosePane {
        pane: MuxPaneId,
    },
    CloseTab {
        tab: MuxTabId,
    },
    SelectTab {
        tab: MuxTabId,
    },
    /// active pane 변경 — Viewport push 대상(14.4)이 바뀐다
    FocusPane {
        pane: MuxPaneId,
    },
    /// 이전 실행이 저장한 mux layout을 복원한다 (PR-14, 설계문서 §11.1~11.5·§14).
    /// 앱이 subscribe 직후 1회 보낸다 — subscribe→restore 순서와 "빈 상태" 전제를
    /// 코드로 보장하기 위해 worker 자율 복원이 아닌 명시적 명령으로 트리거한다.
    /// worker는 세션이 하나도 없을 때만 복원한다(이미 SpawnShell 등이 처리됐으면 skip).
    RestoreWorkspace,
    /// workspace 런타임 상태 전환 (§14.1). Active면 snapshot 생성, 그 외는 중단.
    /// **enum 끝에 append** — postcard는 variant를 index로 인코딩하므로 중간 삽입은
    /// 기존 명령의 discriminant를 밀어 remote wire 호환을 깬다 (codex 리뷰).
    SetWorkspaceState(WorkspaceRuntimeState),
    /// split 경계 마우스 드래그 리사이즈 — tab layout 안 Split을 루트 기준
    /// path(0=first/1=second)로 지정해 ratio를 바꾼다. stale path(레이아웃이 그 사이
    /// 바뀜)는 무해하게 무시된다. (append-only — wire 호환)
    ResizeSplit {
        tab: MuxTabId,
        path: Vec<u8>,
        ratio: f32,
    },
    /// User status override. Existing detector events remain unchanged.
    SetUserStatusOverride {
        session: SessionId,
        override_: session::UserStatusOverride,
    },
    /// pane 제목을 바꾼다(세션 이름 rename). mux.panes의 title을 갱신하고 영속한다.
    /// (append-only — wire 호환)
    RenamePane {
        pane: MuxPaneId,
        title: String,
    },
    /// 이후 SpawnShell이 사용할 워크스페이스 기본 env(.env 자동 주입 — 2026-07-07).
    /// secret은 credential_id 참조로만 전달되고 worker가 spawn 직전에 resolve한다(6.3).
    /// **wire 계약**: postcard enum discriminant라 variant는 항상 끝에만 추가한다(codex High).
    SetSessionDefaultEnv {
        env_plain: Vec<(String, String)>,
        /// (env key, credential_id)
        env_secrets: Vec<(String, String)>,
    },
    /// 이후 SpawnShell/SpawnAgent가 쓸 셸 cwd를 갱신한다(프로젝트 폴더 live 변경 —
    /// 2026-07-08). None이면 앱 cwd 상속. **wire 계약: variant는 끝에만 추가**.
    SetShellCwd(Option<std::path::PathBuf>),
    /// 감지 워커(lsof)가 관측한 세션의 현재 작업 폴더 — persist에 기록해 재시작 복원이
    /// pane별 원래 폴더에서 셸을 띄우게 한다(A안 2026-07-08). **variant는 끝에만 추가**.
    UpdateSessionCwd {
        session: SessionId,
        cwd: String,
    },
    /// exited 백엔드 캐시 정책 (§14.3 확장, 2026-07-11 — 설정에서 변경).
    /// max_exited_backends = live 백엔드 LRU 상한(초과분은 압축 아카이브),
    /// cache_budget_bytes = 이 runtime에 배정된 프로세스 전역 터미널 캐시 예산의 share.
    /// **variant는 끝에만 추가** (wire 계약).
    SetTerminalCachePolicy {
        max_exited_backends: usize,
        cache_budget_bytes: usize,
    },
    /// 원격 시청 lease (모바일 PWA 터미널 뷰어 — v3.3 P5a). viewing=true는 세션을
    /// "visible 등가"로 승격해 hidden tab/Warm에서도 스냅샷을 생성하게 하고, ttl_ms
    /// 안에 갱신(재전송)이 없으면 자동 원복된다 — WS 절단·브리지 사망 백스톱.
    /// viewing=false는 즉시 해제(ttl_ms 무시). **variant는 끝에만 추가** (wire 계약).
    SetRemoteViewing {
        session: SessionId,
        viewing: bool,
        /// lease 유효기간(ms) — worker가 상한 5분으로 캡한다. 시청 유지는 재전송으로 갱신.
        ttl_ms: u32,
    },
    /// 터미널 텍스트 검색 (T3) — 세션의 scrollback+화면 전체에서 query를 대소문자
    /// 무시로 찾는다. worker가 backend에서 검색해 `ScrollbackSearchResult`를 이벤트로
    /// 회신한다. **variant는 끝에만 추가** (postcard discriminant — remote wire 호환).
    SearchScrollback {
        session: SessionId,
        query: String,
        /// 매치 수 상한 — 도달 시 결과가 잘린다(대형 scrollback 방어).
        max_matches: u32,
    },
    /// 스크롤백에서 맨 아래(라이브 화면)로 복귀 — pane 메뉴/단축키(⌘↓)용.
    ScrollToBottom {
        session: SessionId,
    },
    /// OSC 133 프롬프트 마크로 스크롤 점프 (셸 통합 1단계 — 단축키 ⌘⇧↑/↓).
    /// direction −1=이전(과거)/+1=다음(최신). 마크는 세션(워커)이 출력 스트림에서
    /// 스캔해 보관한다. **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    ScrollToPrompt {
        session: SessionId,
        direction: i8,
    },
    /// 마지막 명령 출력(OSC 133 C~D 범위) 추출 요청 (셸 통합 2단계 — pane 메뉴
    /// 「마지막 출력 복사/에이전트로」). 응답은 `RuntimeEvent::LastOutputExtracted` —
    /// 마크가 없으면 빈 text로 회신한다(판정은 UI 몫).
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    ExtractLastOutput {
        session: SessionId,
    },
    /// 시스템 메모리 압박 시 비상 플러시 (로드맵 C2) — DbWriteWorker의 debounce
    /// 배치(세션 status/log offset)를 즉시 커밋한다. OOM-kill은 Drop을 실행하지
    /// 않으므로 압박 신호 시점의 이 명령이 유일한 사전 안전망이다.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    EmergencyPersistFlush,
    /// 폭주 세션 프로세스 그룹 동결(SIGSTOP) — 사용자 조치 (로드맵 B3). 자동 해제
    /// 없음. 결과는 `RuntimeEvent::SessionFreezeChanged`로 회신한다.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    FreezeSession {
        session: SessionId,
    },
    /// 동결된 세션 재개(SIGCONT) — 사용자 조치 (로드맵 B3).
    ResumeSession {
        session: SessionId,
    },
    /// hook이 보고한 새 턴 시작(UserPromptSubmit/PreToolUse) — status detector에
    /// 사용자 입력과 동일한 리셋을 건다. regex 결과 상태(Error/Done)는 latch라 해제가
    /// on_input(=pane에 직접 타이핑)뿐이었고, 그래서 오탐 한 번이 그 pane에 무기한
    /// 남았다. 턴 경계는 latch를 끝낼 정당한 신호다.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    NoteTurnStart {
        session: SessionId,
    },
    /// 저장된 canonical pane 하나만 materialize한다. 첫 요청은 bounded restore
    /// snapshot의 tab/layout/pane skeleton을 설치하고, 지정 pane만 기존 복원 경로로
    /// 세션을 붙인다. **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    RestoreWorkspacePane {
        pane: MuxPaneId,
    },
    /// FIFO marker proving that durable lifecycle/mux events synchronously emitted by
    /// earlier commands have entered their bounded FIFO channel. Coalesced Viewport,
    /// PtyInputPressure, and ResourceUsage slots are explicitly outside this fence.
    /// The opaque id is fixed-size and nonzero; issuers sharing one runtime backend
    /// must keep their outstanding ids unique.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    DurableEventBarrier {
        correlation_id: u64,
    },
    /// 현재 워커가 소유하지만 mux pane 및 원격 시청 lease에 연결되지 않은 로컬
    /// 세션 수를 런타임 상태에서 계산한다. **variant는 끝에만 추가** (wire 계약).
    InspectUnattachedSessions,
    /// 실행 시점에 unattached 후보를 다시 계산해 런타임 소유 세션만 정리한다.
    /// UI가 session id를 전달하지 않는다. **variant는 끝에만 추가** (wire 계약).
    KillUnattachedSessions,
    /// 열람 전용으로 복원된(archived) 에이전트 세션을 그 pane 자리에서 재실행한다
    /// (PR-2). 사용자가 pane의 「다시 실행」을 눌렀을 때만 온다 — 자동 재실행은 없다.
    /// persistence 헤더가 금지하는 건 restore 시점의 **자동** 재실행이지, 명시적
    /// 사용자 확인까지 막는 건 아니다(2026-08-11 정책 변경). 대상이 archived agent
    /// pane이 아니면(라이브 세션·미존재 세션 등) worker가 안전하게 실패로 처리한다.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 계약).
    RespawnArchivedAgent {
        session: SessionId,
        /// 저장된 launch args 뒤에 덧붙일 이어가기 인자(`--continue` 등). 비어 있으면
        /// 새 대화. 판정은 app 쪽 `agent_resume::resume_args`가 하고 여기선 받기만
        /// 한다 — 저장된 launch spec 자체(영속 args)는 바뀌지 않는다(재실행을 거듭해도
        /// 인자가 누적되지 않게).
        extra_args: Vec<String>,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    },
    /// 기존·향후 세션의 사용자 보관 한도. generation은 호출자가 결과를 연결하는 식별자다.
    /// **variant는 끝에만 추가** (postcard discriminant — wire 호환).
    SetScrollbackLimit {
        generation: u64,
        requested: u32,
    },
}

impl std::fmt::Debug for RuntimeCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeCommand::SpawnShell {
                cols,
                rows,
                scrollback_lines,
            } => f
                .debug_struct("SpawnShell")
                .field("cols", cols)
                .field("rows", rows)
                .field("scrollback_lines", scrollback_lines)
                .finish(),
            RuntimeCommand::SpawnAgent {
                cols,
                rows,
                scrollback_lines,
                agent_config_id,
                command,
                args,
                env_plain,
                env_secrets,
                waiting_regex,
                approval_regex,
                error_regex,
                done_regex,
            } => f
                .debug_struct("SpawnAgent")
                .field("cols", cols)
                .field("rows", rows)
                .field("scrollback_lines", scrollback_lines)
                .field("agent_config_id_set", &agent_config_id.is_some())
                .field("command_len", &command.len())
                .field("args_count", &args.len())
                .field("env_plain_count", &env_plain.len())
                .field("env_secret_count", &env_secrets.len())
                .field("waiting_regex_set", &waiting_regex.is_some())
                .field("approval_regex_set", &approval_regex.is_some())
                .field("error_regex_set", &error_regex.is_some())
                .field("done_regex_set", &done_regex.is_some())
                .finish(),
            RuntimeCommand::SetSessionDefaultEnv {
                env_plain,
                env_secrets,
            } => f
                .debug_struct("SetSessionDefaultEnv")
                .field("env_plain_count", &env_plain.len())
                .field("env_secret_count", &env_secrets.len())
                .finish(),
            RuntimeCommand::SetShellCwd(cwd) => {
                f.debug_tuple("SetShellCwd").field(&cwd.is_some()).finish()
            }
            RuntimeCommand::UpdateSessionCwd { session, .. } => f
                .debug_struct("UpdateSessionCwd")
                .field("session", session)
                .finish_non_exhaustive(),
            RuntimeCommand::SetTerminalCachePolicy {
                max_exited_backends,
                cache_budget_bytes,
            } => f
                .debug_struct("SetTerminalCachePolicy")
                .field("max_exited_backends", max_exited_backends)
                .field("cache_budget_bytes", cache_budget_bytes)
                .finish(),
            RuntimeCommand::WriteInput { session, bytes } => f
                .debug_struct("WriteInput")
                .field("session", session)
                .field("bytes_len", &bytes.len())
                .finish(),
            RuntimeCommand::Resize {
                session,
                cols,
                rows,
            } => f
                .debug_struct("Resize")
                .field("session", session)
                .field("cols", cols)
                .field("rows", rows)
                .finish(),
            RuntimeCommand::Scroll { session, delta } => f
                .debug_struct("Scroll")
                .field("session", session)
                .field("delta", delta)
                .finish(),
            RuntimeCommand::ScrollToBottom { session } => f
                .debug_struct("ScrollToBottom")
                .field("session", session)
                .finish(),
            RuntimeCommand::ScrollToPrompt { session, direction } => f
                .debug_struct("ScrollToPrompt")
                .field("session", session)
                .field("direction", direction)
                .finish(),
            RuntimeCommand::ExtractLastOutput { session } => f
                .debug_struct("ExtractLastOutput")
                .field("session", session)
                .finish(),
            RuntimeCommand::EmergencyPersistFlush => {
                f.debug_struct("EmergencyPersistFlush").finish()
            }
            RuntimeCommand::FreezeSession { session } => f
                .debug_struct("FreezeSession")
                .field("session", session)
                .finish(),
            RuntimeCommand::ResumeSession { session } => f
                .debug_struct("ResumeSession")
                .field("session", session)
                .finish(),
            RuntimeCommand::NoteTurnStart { session } => f
                .debug_struct("NoteTurnStart")
                .field("session", session)
                .finish(),
            RuntimeCommand::KillSession { session } => f
                .debug_struct("KillSession")
                .field("session", session)
                .finish(),
            RuntimeCommand::SeedRedaction { credential_ids } => f
                .debug_struct("SeedRedaction")
                .field("credential_count", &credential_ids.len())
                .finish(),
            RuntimeCommand::SplitPane {
                pane,
                direction,
                scrollback_lines,
            } => f
                .debug_struct("SplitPane")
                .field("pane", pane)
                .field("direction", direction)
                .field("scrollback_lines", scrollback_lines)
                .finish(),
            RuntimeCommand::ClosePane { pane } => {
                f.debug_struct("ClosePane").field("pane", pane).finish()
            }
            RuntimeCommand::CloseTab { tab } => {
                f.debug_struct("CloseTab").field("tab", tab).finish()
            }
            RuntimeCommand::SelectTab { tab } => {
                f.debug_struct("SelectTab").field("tab", tab).finish()
            }
            RuntimeCommand::FocusPane { pane } => {
                f.debug_struct("FocusPane").field("pane", pane).finish()
            }
            RuntimeCommand::RestoreWorkspace => f.write_str("RestoreWorkspace"),
            RuntimeCommand::RestoreWorkspacePane { pane } => f
                .debug_struct("RestoreWorkspacePane")
                .field("pane", pane)
                .finish(),
            RuntimeCommand::DurableEventBarrier { correlation_id } => f
                .debug_struct("DurableEventBarrier")
                .field("correlation_id", correlation_id)
                .finish(),
            RuntimeCommand::SetScrollbackLimit {
                generation,
                requested,
            } => f
                .debug_struct("SetScrollbackLimit")
                .field("generation", generation)
                .field("requested", requested)
                .finish(),
            RuntimeCommand::InspectUnattachedSessions => f.write_str("InspectUnattachedSessions"),
            RuntimeCommand::KillUnattachedSessions => f.write_str("KillUnattachedSessions"),
            RuntimeCommand::RespawnArchivedAgent {
                session,
                extra_args,
                cols,
                rows,
                scrollback_lines,
            } => f
                .debug_struct("RespawnArchivedAgent")
                .field("session", session)
                .field("extra_args_count", &extra_args.len())
                .field("cols", cols)
                .field("rows", rows)
                .field("scrollback_lines", scrollback_lines)
                .finish(),
            RuntimeCommand::SetWorkspaceState(state) => {
                f.debug_tuple("SetWorkspaceState").field(state).finish()
            }
            RuntimeCommand::ResizeSplit { tab, path, ratio } => f
                .debug_struct("ResizeSplit")
                .field("tab", tab)
                .field("path_len", &path.len())
                .field("ratio", ratio)
                .finish(),
            RuntimeCommand::SetUserStatusOverride { session, override_ } => f
                .debug_struct("SetUserStatusOverride")
                .field("session", session)
                .field("override", override_)
                .finish(),
            RuntimeCommand::RenamePane { pane, title } => f
                .debug_struct("RenamePane")
                .field("pane", pane)
                .field("title_len", &title.len())
                .finish(),
            RuntimeCommand::SetRemoteViewing {
                session,
                viewing,
                ttl_ms,
            } => f
                .debug_struct("SetRemoteViewing")
                .field("session", session)
                .field("viewing", viewing)
                .field("ttl_ms", ttl_ms)
                .finish(),
            // query 내용은 로그에 남기지 않는다(붙여넣기한 비밀 방어) — 길이만.
            RuntimeCommand::SearchScrollback {
                session,
                query,
                max_matches,
            } => f
                .debug_struct("SearchScrollback")
                .field("session", session)
                .field("query_len", &query.len())
                .field("max_matches", max_matches)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_agent_command() -> RuntimeCommand {
        RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
            agent_config_id: Some("agent-id".to_owned()),
            command: "x".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        }
    }

    #[test]
    fn live_scrollback_명령은_설정범위와_세대번호를_검증한다() {
        for requested in [100, 999, 100_000] {
            let command = RuntimeCommand::SetScrollbackLimit {
                generation: 7,
                requested,
            };
            assert!(validate_host_command(&command).is_ok());
            let bytes = postcard::to_allocvec(&command).unwrap();
            let decoded: RuntimeCommand = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, command);
        }
        for (generation, requested) in [(0, 100), (1, 0), (1, 99), (1, 100_001)] {
            assert!(
                validate_host_command(&RuntimeCommand::SetScrollbackLimit {
                    generation,
                    requested
                })
                .is_err()
            );
        }
    }

    #[test]
    fn runtime_command_debug는_spawn_agent와_input_payload를_숨긴다() {
        let command = RuntimeCommand::SpawnAgent {
            cols: 120,
            rows: 40,
            scrollback_lines: 10_000,
            agent_config_id: Some("agent-secret-id".to_owned()),
            command: "command-debug-never-log".to_owned(),
            args: vec!["--token".to_owned(), "sk-debug-never-log".to_owned()],
            env_plain: vec![("API_KEY".to_owned(), "plain-debug-never-log".to_owned())],
            env_secrets: vec![("SECRET".to_owned(), "cred-debug-never-log".to_owned())],
            waiting_regex: Some("waiting-secret-pattern".to_owned()),
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        let input = RuntimeCommand::WriteInput {
            session: SessionId(7),
            bytes: b"paste-debug-never-log".to_vec(),
        };
        let seed = RuntimeCommand::SeedRedaction {
            credential_ids: vec!["cred-seed-never-log".to_owned()],
        };

        let text = format!("{command:?}\n{input:?}\n{seed:?}");

        for forbidden in [
            "agent-secret-id",
            "command-debug-never-log",
            "sk-debug-never-log",
            "plain-debug-never-log",
            "cred-debug-never-log",
            "waiting-secret-pattern",
            "paste-debug-never-log",
            "cred-seed-never-log",
            "API_KEY",
            "SECRET",
        ] {
            assert!(
                !text.contains(forbidden),
                "Debug leaked {forbidden}: {text}"
            );
        }
        assert!(text.contains("args_count"));
        assert!(text.contains("bytes_len"));
        assert!(text.contains("credential_count"));
    }

    #[test]
    fn spawn_agent_postcard_golden_bytes_are_unchanged() {
        let command = RuntimeCommand::SpawnAgent {
            cols: 1,
            rows: 1,
            scrollback_lines: 0,
            agent_config_id: Some("a".to_owned()),
            command: "x".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };

        assert_eq!(
            postcard::to_allocvec(&command).unwrap(),
            vec![1, 1, 1, 0, 1, 1, b'a', 1, b'x', 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn host_validation_bounds_opaque_agent_config_id() {
        let command_with = |id: String| RuntimeCommand::SpawnAgent {
            cols: 1,
            rows: 1,
            scrollback_lines: 0,
            agent_config_id: Some(id),
            command: "x".to_owned(),
            args: Vec::new(),
            env_plain: Vec::new(),
            env_secrets: Vec::new(),
            waiting_regex: None,
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };

        assert!(validate_host_command(&command_with("a".repeat(128))).is_ok());
        assert!(validate_host_command(&command_with(String::new())).is_err());
        assert!(validate_host_command(&command_with("a".repeat(129))).is_err());
        assert!(validate_host_command(&command_with("a\0b".to_owned())).is_err());
        assert!(validate_host_command(&command_with("a\nb".to_owned())).is_err());
    }

    #[test]
    fn command_admission_accepts_exact_scalar_limits_and_rejects_plus_one() {
        assert!(
            validate_host_command(&RuntimeCommand::SpawnShell {
                cols: 256,
                rows: 256,
                scrollback_lines: SCROLLBACK_LINES_MAX,
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::SpawnShell {
                cols: TERMINAL_DIMENSION_MAX,
                rows: 100,
                scrollback_lines: 0,
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::SpawnShell {
                cols: 100,
                rows: TERMINAL_DIMENSION_MAX,
                scrollback_lines: 0,
            })
            .is_ok()
        );
        for invalid in [
            RuntimeCommand::SpawnShell {
                cols: 0,
                rows: 1,
                scrollback_lines: 0,
            },
            RuntimeCommand::SpawnShell {
                cols: TERMINAL_DIMENSION_MAX + 1,
                rows: 1,
                scrollback_lines: 0,
            },
            RuntimeCommand::SpawnShell {
                cols: 1,
                rows: 1,
                scrollback_lines: SCROLLBACK_LINES_MAX + 1,
            },
            RuntimeCommand::SpawnShell {
                cols: 257,
                rows: 256,
                scrollback_lines: 0,
            },
        ] {
            assert!(validate_host_command(&invalid).is_err());
        }

        let mut agent = valid_agent_command();
        if let RuntimeCommand::SpawnAgent { command, .. } = &mut agent {
            *command = "x".repeat(COMMAND_BYTES_MAX);
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { command, .. } = &mut agent {
            command.push('x');
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { command, .. } = &mut agent {
            command.clear();
        }
        assert!(validate_host_command(&agent).is_err());

        let exact_path = std::path::PathBuf::from("x".repeat(PATH_BYTES_MAX));
        assert!(validate_host_command(&RuntimeCommand::SetShellCwd(Some(exact_path))).is_ok());
        assert!(
            validate_host_command(&RuntimeCommand::SetShellCwd(Some(
                std::path::PathBuf::from("x".repeat(PATH_BYTES_MAX + 1)),
            )))
            .is_err()
        );
        assert!(
            validate_host_command(&RuntimeCommand::SetShellCwd(Some(
                std::path::PathBuf::new(),
            )))
            .is_err()
        );

        assert!(
            validate_host_command(&RuntimeCommand::RenamePane {
                pane: MuxPaneId::new(),
                title: "x".repeat(PANE_TITLE_BYTES_MAX),
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::RenamePane {
                pane: MuxPaneId::new(),
                title: "x".repeat(PANE_TITLE_BYTES_MAX + 1),
            })
            .is_err()
        );

        assert!(
            validate_host_command(&RuntimeCommand::ResizeSplit {
                tab: MuxTabId::new(),
                path: vec![0; SPLIT_PATH_ITEMS_MAX],
                ratio: 1.0,
            })
            .is_ok()
        );
        for (path, ratio) in [
            (vec![0; SPLIT_PATH_ITEMS_MAX + 1], 0.5),
            (Vec::new(), f32::NAN),
            (Vec::new(), 1.01),
        ] {
            assert!(
                validate_host_command(&RuntimeCommand::ResizeSplit {
                    tab: MuxTabId::new(),
                    path,
                    ratio,
                })
                .is_err()
            );
        }

        assert!(
            validate_host_command(&RuntimeCommand::SearchScrollback {
                session: SessionId(1),
                query: "x".repeat(SEARCH_QUERY_BYTES_MAX),
                max_matches: 1,
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::SearchScrollback {
                session: SessionId(1),
                query: "x".repeat(SEARCH_QUERY_BYTES_MAX + 1),
                max_matches: 1,
            })
            .is_err()
        );

        assert!(
            validate_host_command(&RuntimeCommand::WriteInput {
                session: SessionId(1),
                bytes: vec![0; WRITE_INPUT_BYTES_MAX],
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::WriteInput {
                session: SessionId(1),
                bytes: vec![0; WRITE_INPUT_BYTES_MAX + 1],
            })
            .is_err()
        );
    }

    #[test]
    fn command_admission_bounds_args_env_regex_and_seed_aggregates() {
        let mut agent = valid_agent_command();
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut agent {
            *args = vec!["x".repeat(ARG_BYTES_MAX); ARG_AGGREGATE_BYTES_MAX / ARG_BYTES_MAX];
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut agent {
            args.push("x".to_owned());
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut agent {
            *args = vec![String::new(); ARG_ITEMS_MAX];
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut agent {
            args.push(String::new());
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { args, .. } = &mut agent {
            *args = vec!["x".repeat(ARG_BYTES_MAX + 1)];
        }
        assert!(validate_host_command(&agent).is_err());

        if let RuntimeCommand::SpawnAgent {
            args, env_plain, ..
        } = &mut agent
        {
            args.clear();
            *env_plain = vec![("K".to_owned(), String::new()); ENV_ITEMS_MAX];
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            env_plain.push(("K".to_owned(), String::new()));
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            *env_plain = (0..31)
                .map(|_| ("K".to_owned(), "x".repeat(ENV_VALUE_BYTES_MAX)))
                .chain(std::iter::once((
                    "K".to_owned(),
                    "x".repeat(ENV_AGGREGATE_BYTES_MAX - 32 - 31 * ENV_VALUE_BYTES_MAX),
                )))
                .collect();
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            env_plain.last_mut().unwrap().1.push('x');
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            *env_plain = vec![("K".repeat(ENV_KEY_BYTES_MAX), String::new())];
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            env_plain[0].0.push('K');
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { env_plain, .. } = &mut agent {
            *env_plain = vec![("BAD=KEY".to_owned(), String::new())];
        }
        assert!(validate_host_command(&agent).is_err());

        if let RuntimeCommand::SpawnAgent {
            env_plain,
            env_secrets,
            ..
        } = &mut agent
        {
            env_plain.clear();
            *env_secrets = vec![(
                "SECRET".to_owned(),
                "x".repeat(SEED_CREDENTIAL_ID_BYTES_MAX),
            )];
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { env_secrets, .. } = &mut agent {
            env_secrets[0].1.push('x');
        }
        assert!(validate_host_command(&agent).is_err());
        if let RuntimeCommand::SpawnAgent { env_secrets, .. } = &mut agent {
            env_secrets[0].1 = "bad\nid".to_owned();
        }
        assert!(validate_host_command(&agent).is_err());

        if let RuntimeCommand::SpawnAgent {
            env_secrets,
            waiting_regex,
            approval_regex,
            error_regex,
            done_regex,
            ..
        } = &mut agent
        {
            env_secrets.clear();
            *waiting_regex = Some("x".repeat(REGEX_BYTES_MAX));
            *approval_regex = Some("x".repeat(REGEX_BYTES_MAX));
            *error_regex = Some("x".repeat(REGEX_BYTES_MAX));
            *done_regex = Some("x".repeat(REGEX_BYTES_MAX));
        }
        assert!(validate_host_command(&agent).is_ok());
        if let RuntimeCommand::SpawnAgent { done_regex, .. } = &mut agent {
            done_regex.as_mut().unwrap().push('x');
        }
        assert!(validate_host_command(&agent).is_err());

        assert!(
            validate_host_command(&RuntimeCommand::SeedRedaction {
                credential_ids: vec![
                    "x".repeat(SEED_CREDENTIAL_ID_BYTES_MAX);
                    SEED_CREDENTIAL_ITEMS_MAX
                ],
            })
            .is_ok()
        );
        assert!(
            validate_host_command(&RuntimeCommand::SeedRedaction {
                credential_ids: vec!["x".to_owned(); SEED_CREDENTIAL_ITEMS_MAX + 1],
            })
            .is_err()
        );
    }

    #[test]
    fn internal_env_slot_preserves_exact_external_limit() {
        let mut env = vec![("K".to_owned(), String::new()); ENV_ITEMS_MAX];
        env.push(("DEPPY_SESSION_ID".to_owned(), "workspace:1".to_owned()));
        assert!(
            validate_launch_spec_with_internal_env("x", &[], &env, None, "DEPPY_SESSION_ID",)
                .is_ok()
        );
        env.insert(0, ("EXTRA".to_owned(), String::new()));
        assert!(
            validate_launch_spec_with_internal_env("x", &[], &env, None, "DEPPY_SESSION_ID",)
                .is_err()
        );
        env.pop();
        env.push(("DEPPY_SESSION_ID".to_owned(), "duplicate".to_owned()));
        assert!(
            validate_launch_spec_with_internal_env("x", &[], &env, None, "DEPPY_SESSION_ID",)
                .is_err()
        );
    }

    fn mux_id_commands(id: &str) -> Vec<RuntimeCommand> {
        let pane = || MuxPaneId(id.to_owned());
        let tab = || MuxTabId(id.to_owned());
        vec![
            RuntimeCommand::SplitPane {
                pane: pane(),
                direction: SplitDirection::Horizontal,
                scrollback_lines: 0,
            },
            RuntimeCommand::ClosePane { pane: pane() },
            RuntimeCommand::FocusPane { pane: pane() },
            RuntimeCommand::RestoreWorkspacePane { pane: pane() },
            RuntimeCommand::RenamePane {
                pane: pane(),
                title: String::new(),
            },
            RuntimeCommand::CloseTab { tab: tab() },
            RuntimeCommand::SelectTab { tab: tab() },
            RuntimeCommand::ResizeSplit {
                tab: tab(),
                path: Vec::new(),
                ratio: 0.5,
            },
        ]
    }

    #[test]
    fn every_mux_id_variant_accepts_exact_and_rejects_plus_one_or_control() {
        for command in mux_id_commands(&"x".repeat(MUX_ID_BYTES_MAX)) {
            assert!(validate_host_command(&command).is_ok(), "{command:?}");
        }
        for invalid in [
            String::new(),
            "x".repeat(MUX_ID_BYTES_MAX + 1),
            "bad\nid".to_owned(),
            "bad\0id".to_owned(),
        ] {
            for command in mux_id_commands(&invalid) {
                assert!(validate_host_command(&command).is_err(), "{command:?}");
            }
        }
    }

    #[test]
    fn restore_workspace_pane_debug_exposes_only_bounded_identifier() {
        let command = RuntimeCommand::RestoreWorkspacePane {
            pane: MuxPaneId("pane-safe".to_owned()),
        };

        assert_eq!(
            format!("{command:?}"),
            "RestoreWorkspacePane { pane: MuxPaneId(\"pane-safe\") }"
        );
    }

    fn spare_string(value: &str, capacity: usize) -> String {
        let mut output = String::with_capacity(capacity);
        output.push_str(value);
        output
    }

    #[test]
    fn retention_preflight_validates_then_canonicalizes_then_measures_capacity() {
        let mut command = valid_agent_command();
        let RuntimeCommand::SpawnAgent {
            command: program, ..
        } = &mut command
        else {
            unreachable!()
        };
        *program = spare_string("x", RUNTIME_COMMAND_QUEUE_BYTES_MAX * 2);
        assert!(
            runtime_command_retained_bytes(&command).unwrap() > RUNTIME_COMMAND_QUEUE_BYTES_MAX
        );

        let retention = prepare_runtime_command_for_retention(&mut command).unwrap();
        assert!(retention.retained_bytes() <= RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        assert_eq!(
            retention.retained_bytes(),
            runtime_command_retained_bytes(&command).unwrap()
        );
        let RuntimeCommand::SpawnAgent {
            command: program, ..
        } = &command
        else {
            unreachable!()
        };
        assert_eq!(program.capacity(), program.len());
        assert_eq!(format!("{retention:?}"), "RuntimeCommandRetention");

        for invalid_program in [
            spare_string("", 64 * 1024),
            spare_string("bad\0program", 64 * 1024),
            "x".repeat(COMMAND_BYTES_MAX + 1),
        ] {
            let mut invalid = valid_agent_command();
            let RuntimeCommand::SpawnAgent {
                command: program, ..
            } = &mut invalid
            else {
                unreachable!()
            };
            *program = invalid_program;
            let original_capacity = program.capacity();
            assert_eq!(
                prepare_runtime_command_for_retention(&mut invalid),
                Err(RuntimeCommandPreparationErrorCode::InvalidCommand)
            );
            let RuntimeCommand::SpawnAgent {
                command: program, ..
            } = &invalid
            else {
                unreachable!()
            };
            assert_eq!(program.capacity(), original_capacity);
        }
        assert_eq!(
            format!("{:?}", RuntimeCommandPreparationErrorCode::InvalidCommand),
            "invalid_command"
        );
        assert_eq!(
            format!("{:?}", RuntimeCommandPreparationErrorCode::ResourceLimit),
            "resource_limit"
        );
    }

    #[test]
    fn retention_preflight_accepts_a_maximum_valid_agent_command() {
        let argument_count = ARG_AGGREGATE_BYTES_MAX / ARG_BYTES_MAX;
        let environment_count = ENV_AGGREGATE_BYTES_MAX / ENV_VALUE_BYTES_MAX;
        let final_environment_value = ENV_AGGREGATE_BYTES_MAX
            - environment_count
            - (environment_count - 1) * ENV_VALUE_BYTES_MAX;
        let mut command = RuntimeCommand::SpawnAgent {
            cols: 256,
            rows: 256,
            scrollback_lines: SCROLLBACK_LINES_MAX,
            agent_config_id: Some("a".repeat(AGENT_CONFIG_ID_MAX_BYTES)),
            command: "x".repeat(COMMAND_BYTES_MAX),
            args: vec!["x".repeat(ARG_BYTES_MAX); argument_count],
            env_plain: (0..environment_count)
                .map(|index| {
                    let value_bytes = if index + 1 == environment_count {
                        final_environment_value
                    } else {
                        ENV_VALUE_BYTES_MAX
                    };
                    ("K".to_owned(), "x".repeat(value_bytes))
                })
                .collect(),
            env_secrets: Vec::new(),
            waiting_regex: Some("x".repeat(REGEX_BYTES_MAX)),
            approval_regex: Some("x".repeat(REGEX_BYTES_MAX)),
            error_regex: Some("x".repeat(REGEX_BYTES_MAX)),
            done_regex: Some("x".repeat(REGEX_BYTES_MAX)),
        };

        let retention = prepare_runtime_command_for_retention(&mut command).unwrap();
        assert!(retention.retained_bytes() <= RUNTIME_COMMAND_QUEUE_BYTES_MAX);
        assert_eq!(
            retention.retained_bytes(),
            runtime_command_retained_bytes(&command).unwrap()
        );
    }

    #[test]
    fn retention_aggregate_uses_the_exact_runtime_queue_ceiling() {
        assert_eq!(
            checked_runtime_command_retention_total(0, RUNTIME_COMMAND_QUEUE_BYTES_MAX),
            Ok(RUNTIME_COMMAND_QUEUE_BYTES_MAX)
        );
        assert_eq!(
            checked_runtime_command_retention_total(RUNTIME_COMMAND_QUEUE_BYTES_MAX, 1),
            Err(RuntimeCommandPreparationErrorCode::ResourceLimit)
        );
        assert_eq!(
            checked_runtime_command_retention_total(usize::MAX, 1),
            Err(RuntimeCommandPreparationErrorCode::ResourceLimit)
        );
    }

    #[test]
    fn canonicalization_removes_spare_capacity_before_long_lived_storage() {
        let mut agent = RuntimeCommand::SpawnAgent {
            cols: 80,
            rows: 24,
            scrollback_lines: 0,
            agent_config_id: Some(spare_string("agent-id", 64 * 1024)),
            command: spare_string("command", 64 * 1024),
            args: vec![spare_string("arg", 64 * 1024)],
            env_plain: vec![(
                spare_string("KEY", 64 * 1024),
                spare_string("value", 64 * 1024),
            )],
            env_secrets: Vec::new(),
            waiting_regex: Some(spare_string("waiting", 64 * 1024)),
            approval_regex: None,
            error_regex: None,
            done_regex: None,
        };
        assert!(validate_host_command(&agent).is_ok());
        canonicalize_host_command(&mut agent);
        let RuntimeCommand::SpawnAgent {
            agent_config_id,
            command,
            args,
            env_plain,
            waiting_regex,
            ..
        } = &agent
        else {
            unreachable!()
        };
        assert_eq!(
            agent_config_id.as_ref().unwrap().capacity(),
            "agent-id".len()
        );
        assert_eq!(command.capacity(), command.len());
        assert_eq!(args.capacity(), args.len());
        assert_eq!(args[0].capacity(), args[0].len());
        assert_eq!(env_plain.capacity(), env_plain.len());
        assert_eq!(env_plain[0].0.capacity(), env_plain[0].0.len());
        assert_eq!(env_plain[0].1.capacity(), env_plain[0].1.len());
        assert_eq!(
            waiting_regex.as_ref().unwrap().capacity(),
            waiting_regex.as_ref().unwrap().len()
        );

        let mut defaults = Vec::with_capacity(4_096);
        defaults.push((
            spare_string("KEY", 64 * 1024),
            spare_string("value", 64 * 1024),
        ));
        let mut command = RuntimeCommand::SetSessionDefaultEnv {
            env_plain: defaults,
            env_secrets: Vec::with_capacity(4_096),
        };
        canonicalize_host_command(&mut command);
        let RuntimeCommand::SetSessionDefaultEnv {
            env_plain,
            env_secrets,
        } = command
        else {
            unreachable!()
        };
        assert_eq!(env_plain.capacity(), env_plain.len());
        assert_eq!(env_plain[0].0.capacity(), env_plain[0].0.len());
        assert_eq!(env_plain[0].1.capacity(), env_plain[0].1.len());
        assert_eq!(env_secrets.capacity(), 0);

        let mut cwd = std::path::PathBuf::with_capacity(64 * 1024);
        cwd.push("cwd");
        let mut command = RuntimeCommand::SetShellCwd(Some(cwd));
        canonicalize_host_command(&mut command);
        let RuntimeCommand::SetShellCwd(Some(cwd)) = command else {
            unreachable!()
        };
        assert_eq!(cwd.capacity(), cwd.as_os_str().as_encoded_bytes().len());
    }

    #[test]
    fn runtime_command_variant_order_is_source_locked() {
        let source = include_str!("command.rs");
        let body = source
            .split_once("pub enum RuntimeCommand {")
            .unwrap()
            .1
            .split_once("\n}\n\nimpl std::fmt::Debug for RuntimeCommand")
            .unwrap()
            .0;
        let actual = body
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("    ")?;
                if line.starts_with([' ', '/']) {
                    return None;
                }
                let name = line
                    .split(|character: char| !character.is_ascii_alphanumeric())
                    .next()?;
                name.chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    .then_some(name)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                "SpawnShell",
                "SpawnAgent",
                "WriteInput",
                "Resize",
                "Scroll",
                "KillSession",
                "SeedRedaction",
                "SplitPane",
                "ClosePane",
                "CloseTab",
                "SelectTab",
                "FocusPane",
                "RestoreWorkspace",
                "SetWorkspaceState",
                "ResizeSplit",
                "SetUserStatusOverride",
                "RenamePane",
                "SetSessionDefaultEnv",
                "SetShellCwd",
                "UpdateSessionCwd",
                "SetTerminalCachePolicy",
                "SetRemoteViewing",
                "SearchScrollback",
                "ScrollToBottom",
                "ScrollToPrompt",
                "ExtractLastOutput",
                "EmergencyPersistFlush",
                "FreezeSession",
                "ResumeSession",
                "NoteTurnStart",
                "RestoreWorkspacePane",
                "DurableEventBarrier",
                "InspectUnattachedSessions",
                "KillUnattachedSessions",
                "RespawnArchivedAgent",
                "SetScrollbackLimit",
            ]
        );
    }

    #[test]
    fn unattached_session_commands_retain_no_heap_payload() {
        for mut command in [
            RuntimeCommand::InspectUnattachedSessions,
            RuntimeCommand::KillUnattachedSessions,
        ] {
            assert!(validate_host_command(&command).is_ok());
            let retained = prepare_runtime_command_for_retention(&mut command).unwrap();
            assert_eq!(
                retained.retained_bytes(),
                std::mem::size_of::<RuntimeCommand>()
            );
        }
    }

    #[test]
    fn durable_event_barrier_requires_nonzero_correlation_and_retains_no_heap() {
        let mut valid = RuntimeCommand::DurableEventBarrier { correlation_id: 7 };
        assert!(validate_host_command(&valid).is_ok());
        let retained = prepare_runtime_command_for_retention(&mut valid).unwrap();
        assert_eq!(
            retained.retained_bytes(),
            std::mem::size_of::<RuntimeCommand>()
        );

        let invalid = RuntimeCommand::DurableEventBarrier { correlation_id: 0 };
        assert!(validate_host_command(&invalid).is_err());
        assert_eq!(
            format!("{valid:?}"),
            "DurableEventBarrier { correlation_id: 7 }"
        );
    }

    fn respawn_archived_agent_command(extra_args: Vec<String>) -> RuntimeCommand {
        RuntimeCommand::RespawnArchivedAgent {
            session: SessionId(1),
            extra_args,
            cols: 80,
            rows: 24,
            scrollback_lines: 100,
        }
    }

    #[test]
    fn respawn_archived_agent_admission_bounds_dimensions_and_extra_args() {
        assert!(validate_host_command(&respawn_archived_agent_command(Vec::new())).is_ok());
        for invalid in [
            RuntimeCommand::RespawnArchivedAgent {
                session: SessionId(1),
                extra_args: Vec::new(),
                cols: 0,
                rows: 24,
                scrollback_lines: 100,
            },
            RuntimeCommand::RespawnArchivedAgent {
                session: SessionId(1),
                extra_args: Vec::new(),
                cols: TERMINAL_DIMENSION_MAX + 1,
                rows: 24,
                scrollback_lines: 100,
            },
            RuntimeCommand::RespawnArchivedAgent {
                session: SessionId(1),
                extra_args: Vec::new(),
                cols: 80,
                rows: 24,
                scrollback_lines: SCROLLBACK_LINES_MAX + 1,
            },
        ] {
            assert!(validate_host_command(&invalid).is_err());
        }

        assert!(
            validate_host_command(&respawn_archived_agent_command(vec![
                String::new();
                ARG_ITEMS_MAX
            ]))
            .is_ok()
        );
        assert!(
            validate_host_command(&respawn_archived_agent_command(vec![
                String::new();
                ARG_ITEMS_MAX + 1
            ]))
            .is_err()
        );
        assert!(
            validate_host_command(&respawn_archived_agent_command(vec![
                "x".repeat(ARG_BYTES_MAX + 1)
            ]))
            .is_err()
        );
    }

    #[test]
    fn respawn_archived_agent_retention_counts_extra_args_and_canonicalizes() {
        let mut command =
            respawn_archived_agent_command(vec![spare_string("--continue", 64 * 1024)]);
        assert!(validate_host_command(&command).is_ok());
        canonicalize_host_command(&mut command);
        let RuntimeCommand::RespawnArchivedAgent { extra_args, .. } = &command else {
            unreachable!()
        };
        assert_eq!(extra_args[0].capacity(), extra_args[0].len());

        let baseline =
            runtime_command_retained_bytes(&respawn_archived_agent_command(Vec::new())).unwrap();
        let with_args = runtime_command_retained_bytes(&respawn_archived_agent_command(vec![
            "--continue".to_owned(),
        ]))
        .unwrap();
        assert!(with_args > baseline);
    }

    #[test]
    fn respawn_archived_agent_debug_hides_extra_args_content() {
        let command = respawn_archived_agent_command(vec!["--continue-secret".to_owned()]);
        let text = format!("{command:?}");
        assert!(!text.contains("--continue-secret"));
        assert!(text.contains("extra_args_count"));
    }
}
