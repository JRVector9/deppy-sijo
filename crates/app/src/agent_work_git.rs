//! Work-history cards에 붙일 현재 Git 사실을 UI 스레드 밖에서 수집한다.
//!
//! 입력과 결과는 각각 최신 요청 하나만 보유한다. 한 요청은 중복 제거된 cwd 최대 16개,
//! cwd마다 절대경로 Git의 bounded `status` 한 번만 실행한다. 새 입력이 오면 이미 실행 중인
//! 이전 generation의 결과는 publish 전에 폐기한다.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) const WORK_HISTORY_GIT_CWDS_MAX: usize = 16;
pub(crate) const WORK_HISTORY_GIT_OUTPUT_MAX: usize = 64 * 1024;
const WORK_HISTORY_GIT_PATH_MAX: usize = 32 * 1024;
const WORK_HISTORY_GIT_INPUT_ROWS_MAX: usize = 256;
const WORK_HISTORY_GIT_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_SPAWN_ERROR: &str = "work_history_git_worker_spawn_failed";
const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v1",
    "--branch",
    "-z",
    "--untracked-files=normal",
];

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WorkHistoryGitFact {
    pub(crate) cwd: String,
    pub(crate) branch: Option<String>,
    pub(crate) changed_files: Option<u32>,
}

impl std::fmt::Debug for WorkHistoryGitFact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkHistoryGitFact")
            .field("cwd", &"REDACTED")
            .field("cwd_bytes", &self.cwd.len())
            .field("has_branch", &self.branch.is_some())
            .field("changed_files", &self.changed_files)
            .finish()
    }
}

pub(crate) struct WorkHistoryGitOutcome {
    pub(crate) generation: u64,
    pub(crate) facts: Arc<[WorkHistoryGitFact]>,
}

impl std::fmt::Debug for WorkHistoryGitOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkHistoryGitOutcome")
            .field("generation", &self.generation)
            .field("facts", &self.facts.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkHistoryGitInputError {
    CwdLimit,
    InvalidPath,
    PathTooLarge,
    WorkerUnavailable,
}

struct ParsedStatus {
    branch: Option<String>,
    changed_files: u32,
}

fn parse_branch_header(record: &str) -> Option<String> {
    let value = record.strip_prefix("## ")?;
    let branch = value
        .strip_prefix("No commits yet on ")
        .or_else(|| value.strip_prefix("Initial commit on "))
        .unwrap_or_else(|| value.split_once("...").map_or(value, |(name, _)| name));
    let branch = branch.split_once(" [").map_or(branch, |(name, _)| name);
    if branch.is_empty() || branch == "HEAD" || branch.starts_with("HEAD (") {
        None
    } else {
        Some(branch.to_owned())
    }
}

fn is_status_record(record: &str) -> bool {
    let bytes = record.as_bytes();
    bytes.len() >= 4
        && bytes[2] == b' '
        && [bytes[0], bytes[1]].into_iter().all(|status| {
            matches!(
                status,
                b' ' | b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'T' | b'?' | b'!'
            )
        })
}

fn parse_porcelain_status(raw: &str) -> ParsedStatus {
    let mut records: Vec<&str> = raw.split('\0').collect();
    if raw.ends_with('\0') {
        records.pop();
    } else if !records.is_empty() {
        // `run_git_bounded`는 잘림을 에러로 돌리지만, parser 자체도 불완전 마지막 record를
        // 상태로 세지 않는다.
        records.pop();
    }

    let branch = records
        .first()
        .and_then(|record| parse_branch_header(record));
    let mut index = usize::from(
        records
            .first()
            .is_some_and(|record| record.starts_with("## ")),
    );
    let mut changed_files = 0u32;
    while index < records.len() {
        let record = records[index];
        if !is_status_record(record) {
            index += 1;
            continue;
        }
        changed_files = changed_files.saturating_add(1);
        let xy = &record.as_bytes()[..2];
        index += 1;
        if xy.contains(&b'R') || xy.contains(&b'C') {
            // porcelain-v1 -z의 rename/copy는 새 경로 record 뒤에 원 경로 record가 하나
            // 더 온다. 원 경로는 같은 logical 변경이므로 건너뛴다.
            index = index.saturating_add(1).min(records.len());
        }
    }
    ParsedStatus {
        branch,
        changed_files,
    }
}

fn collect_git_fact_with(
    cwd: &str,
    run: impl FnOnce(&Path, &[&str], Duration, usize) -> anyhow::Result<String>,
) -> Option<WorkHistoryGitFact> {
    let output = run(
        Path::new(cwd),
        STATUS_ARGS,
        WORK_HISTORY_GIT_TIMEOUT,
        WORK_HISTORY_GIT_OUTPUT_MAX,
    )
    .ok()?;
    let parsed = parse_porcelain_status(&output);
    Some(WorkHistoryGitFact {
        cwd: cwd.to_owned(),
        branch: parsed.branch,
        changed_files: Some(parsed.changed_files),
    })
}

fn collect_git_fact(cwd: &str) -> Option<WorkHistoryGitFact> {
    collect_git_fact_with(cwd, crate::git_cli::run_git_bounded)
}

fn admit_cwds(cwds: Vec<String>) -> Result<Vec<String>, WorkHistoryGitInputError> {
    if cwds.len() > WORK_HISTORY_GIT_INPUT_ROWS_MAX {
        return Err(WorkHistoryGitInputError::CwdLimit);
    }
    let mut admitted = Vec::with_capacity(cwds.len().min(WORK_HISTORY_GIT_CWDS_MAX));
    for cwd in cwds {
        if cwd.is_empty() || cwd.as_bytes().contains(&0) {
            return Err(WorkHistoryGitInputError::InvalidPath);
        }
        if cwd.len() > WORK_HISTORY_GIT_PATH_MAX {
            return Err(WorkHistoryGitInputError::PathTooLarge);
        }
        if admitted.iter().any(|existing| existing == &cwd) {
            continue;
        }
        if admitted.len() == WORK_HISTORY_GIT_CWDS_MAX {
            return Err(WorkHistoryGitInputError::CwdLimit);
        }
        admitted.push(cwd);
    }
    Ok(admitted)
}

struct WorkHistoryGitJob {
    generation: u64,
    cwds: Arc<[String]>,
}

struct VersionedInput {
    revision: u64,
    job: Option<Arc<WorkHistoryGitJob>>,
}

struct InputState {
    current: VersionedInput,
    stopping: bool,
}

struct InputShared {
    state: Mutex<InputState>,
    changed: Condvar,
}

#[derive(Clone)]
pub(crate) struct WorkHistoryGitInput {
    shared: Arc<InputShared>,
    mailbox: Arc<OutcomeMailbox>,
}

impl WorkHistoryGitInput {
    fn new(mailbox: Arc<OutcomeMailbox>) -> Self {
        Self {
            shared: Arc::new(InputShared {
                state: Mutex::new(InputState {
                    current: VersionedInput {
                        revision: 0,
                        job: None,
                    },
                    stopping: false,
                }),
                changed: Condvar::new(),
            }),
            mailbox,
        }
    }

    pub(crate) fn publish(
        &self,
        generation: u64,
        cwds: Vec<String>,
    ) -> Result<(), WorkHistoryGitInputError> {
        let cwds = match admit_cwds(cwds) {
            Ok(cwds) => cwds,
            Err(error) => {
                self.invalidate()?;
                return Err(error);
            }
        };
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| WorkHistoryGitInputError::WorkerUnavailable)?;
        if state.stopping {
            return Err(WorkHistoryGitInputError::WorkerUnavailable);
        }
        if state.current.job.as_ref().is_some_and(|current| {
            current.generation == generation && current.cwds.as_ref() == cwds.as_slice()
        }) {
            return Ok(());
        }
        let revision = state.current.revision.wrapping_add(1);
        state.current = VersionedInput {
            revision,
            job: Some(Arc::new(WorkHistoryGitJob {
                generation,
                cwds: cwds.into(),
            })),
        };
        self.mailbox.invalidate(revision);
        self.shared.changed.notify_one();
        Ok(())
    }

    fn invalidate(&self) -> Result<(), WorkHistoryGitInputError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| WorkHistoryGitInputError::WorkerUnavailable)?;
        if state.stopping {
            return Err(WorkHistoryGitInputError::WorkerUnavailable);
        }
        let revision = state.current.revision.wrapping_add(1);
        state.current = VersionedInput {
            revision,
            job: None,
        };
        self.mailbox.invalidate(revision);
        self.shared.changed.notify_one();
        Ok(())
    }

    fn wait_next(&self, seen_revision: u64) -> Option<VersionedInput> {
        let state = self.shared.state.lock().ok()?;
        let state = self
            .shared
            .changed
            .wait_while(state, |state| {
                !state.stopping && state.current.revision == seen_revision
            })
            .ok()?;
        if state.stopping {
            None
        } else {
            Some(VersionedInput {
                revision: state.current.revision,
                job: state.current.job.clone(),
            })
        }
    }

    fn is_current(&self, revision: u64) -> bool {
        self.shared
            .state
            .lock()
            .is_ok_and(|state| !state.stopping && state.current.revision == revision)
    }

    fn stop(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.stopping = true;
            self.shared.changed.notify_all();
        }
    }
}

struct MailboxState {
    latest: Option<Arc<WorkHistoryGitOutcome>>,
    expected_revision: u64,
    sequence: u64,
    closed: bool,
    consumer_alive: bool,
}

struct OutcomeMailbox {
    state: Mutex<MailboxState>,
    available: Condvar,
}

impl OutcomeMailbox {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MailboxState {
                latest: None,
                expected_revision: 0,
                sequence: 0,
                closed: false,
                consumer_alive: true,
            }),
            available: Condvar::new(),
        })
    }

    fn invalidate(&self, revision: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.expected_revision = revision;
            state.latest = None;
        }
    }

    fn publish(&self, revision: u64, outcome: WorkHistoryGitOutcome) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed || !state.consumer_alive {
            return false;
        }
        if revision != state.expected_revision {
            return true;
        }
        state.latest = Some(Arc::new(outcome));
        state.sequence = state.sequence.wrapping_add(1);
        self.available.notify_one();
        true
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.available.notify_all();
        }
    }
}

pub(crate) struct WorkHistoryGitOutcomeReceiver {
    mailbox: Arc<OutcomeMailbox>,
    seen_sequence: AtomicU64,
}

impl WorkHistoryGitOutcomeReceiver {
    pub(crate) fn try_recv(
        &self,
    ) -> Result<Arc<WorkHistoryGitOutcome>, std::sync::mpsc::TryRecvError> {
        let Ok(state) = self.mailbox.state.lock() else {
            return Err(std::sync::mpsc::TryRecvError::Disconnected);
        };
        let seen = self.seen_sequence.load(Ordering::Relaxed);
        if state.sequence != seen {
            self.seen_sequence.store(state.sequence, Ordering::Relaxed);
            state
                .latest
                .as_ref()
                .map(Arc::clone)
                .ok_or(std::sync::mpsc::TryRecvError::Empty)
        } else if state.closed {
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        } else {
            Err(std::sync::mpsc::TryRecvError::Empty)
        }
    }

    #[cfg(test)]
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Arc<WorkHistoryGitOutcome>, std::sync::mpsc::RecvTimeoutError> {
        let Ok(state) = self.mailbox.state.lock() else {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        };
        let seen = self.seen_sequence.load(Ordering::Relaxed);
        let Ok((state, _)) = self
            .mailbox
            .available
            .wait_timeout_while(state, timeout, |state| {
                state.sequence == seen && !state.closed
            })
        else {
            return Err(std::sync::mpsc::RecvTimeoutError::Disconnected);
        };
        if state.sequence != seen {
            self.seen_sequence.store(state.sequence, Ordering::Relaxed);
            state
                .latest
                .as_ref()
                .map(Arc::clone)
                .ok_or(std::sync::mpsc::RecvTimeoutError::Timeout)
        } else if state.closed {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        } else {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        }
    }
}

impl Drop for WorkHistoryGitOutcomeReceiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.mailbox.state.lock() {
            state.consumer_alive = false;
            self.mailbox.available.notify_all();
        }
    }
}

trait GitFactBackend: Send + 'static {
    fn collect(&mut self, cwd: &str) -> Option<WorkHistoryGitFact>;
}

struct ProductionBackend;

impl GitFactBackend for ProductionBackend {
    fn collect(&mut self, cwd: &str) -> Option<WorkHistoryGitFact> {
        collect_git_fact(cwd)
    }
}

fn run_worker<B: GitFactBackend>(
    input: WorkHistoryGitInput,
    mailbox: Arc<OutcomeMailbox>,
    ctx: egui::Context,
    mut backend: B,
) {
    let mut seen_revision = 0u64;
    while let Some(current) = input.wait_next(seen_revision) {
        seen_revision = current.revision;
        let Some(job) = current.job else {
            continue;
        };
        let mut facts = Vec::with_capacity(job.cwds.len());
        let mut stale = false;
        for cwd in job.cwds.iter() {
            let fact = backend.collect(cwd);
            if !input.is_current(current.revision) {
                stale = true;
                break;
            }
            if let Some(fact) = fact {
                facts.push(fact);
            }
        }
        if stale || !input.is_current(current.revision) {
            continue;
        }
        if !mailbox.publish(
            current.revision,
            WorkHistoryGitOutcome {
                generation: job.generation,
                facts: facts.into(),
            },
        ) {
            break;
        }
        ctx.request_repaint();
    }
    mailbox.close();
}

pub(crate) struct WorkHistoryGitWorker {
    input: WorkHistoryGitInput,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WorkHistoryGitWorker {
    pub(crate) fn spawn(
        ctx: egui::Context,
    ) -> (Self, WorkHistoryGitInput, WorkHistoryGitOutcomeReceiver) {
        Self::spawn_with_backend(ctx, ProductionBackend)
    }

    fn spawn_with_backend<B: GitFactBackend>(
        ctx: egui::Context,
        backend: B,
    ) -> (Self, WorkHistoryGitInput, WorkHistoryGitOutcomeReceiver) {
        let mailbox = OutcomeMailbox::new();
        let input = WorkHistoryGitInput::new(Arc::clone(&mailbox));
        let worker_input = input.clone();
        let worker_mailbox = Arc::clone(&mailbox);
        let handle = std::thread::Builder::new()
            .name("work-history-git".to_owned())
            .spawn(move || run_worker(worker_input, worker_mailbox, ctx, backend))
            .expect(WORKER_SPAWN_ERROR);
        (
            Self {
                input: input.clone(),
                handle: Some(handle),
            },
            input,
            WorkHistoryGitOutcomeReceiver {
                mailbox,
                seen_sequence: AtomicU64::new(0),
            },
        )
    }
}

impl Drop for WorkHistoryGitWorker {
    fn drop(&mut self) {
        self.input.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};

    #[test]
    fn porcelain은_branch형식과_unborn_detached를_구분한다() {
        let normal = parse_porcelain_status(
            "## feature/history...origin/feature/history [ahead 2, behind 1]\0 M src/main.rs\0",
        );
        assert_eq!(normal.branch.as_deref(), Some("feature/history"));
        assert_eq!(normal.changed_files, 1);

        let unborn = parse_porcelain_status("## No commits yet on main\0?? README.md\0");
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert_eq!(unborn.changed_files, 1);

        let detached = parse_porcelain_status("## HEAD (no branch)\0 M src/main.rs\0");
        assert_eq!(detached.branch, None);
        assert_eq!(detached.changed_files, 1);

        let invalid = parse_porcelain_status("not-a-branch-header\0 M src/main.rs\0");
        assert_eq!(invalid.branch, None);
        assert_eq!(invalid.changed_files, 1);
    }

    #[test]
    fn porcelain_z는_rename_copy의_두번째_path를_중복계산하지_않는다() {
        let parsed = parse_porcelain_status(
            "## main\0R  new name.rs\0old\nname.rs\0 C copied.rs\0source.rs\0?? line\nbreak.txt\0 M spaced name.rs\0",
        );
        assert_eq!(parsed.changed_files, 4);
    }

    #[test]
    fn cwd는_중복제거후_정확히_16개만_허용한다() {
        let exact = (0..WORK_HISTORY_GIT_CWDS_MAX)
            .map(|index| format!("/repo/{index}"))
            .collect();
        assert_eq!(admit_cwds(exact).expect("exact limit").len(), 16);

        let with_duplicates = vec!["/repo/a".to_owned(); WORK_HISTORY_GIT_CWDS_MAX + 1];
        assert_eq!(
            admit_cwds(with_duplicates).expect("one distinct cwd").len(),
            1
        );

        let over = (0..=WORK_HISTORY_GIT_CWDS_MAX)
            .map(|index| format!("/repo/{index}"))
            .collect();
        assert_eq!(admit_cwds(over), Err(WorkHistoryGitInputError::CwdLimit));

        assert_eq!(
            admit_cwds(vec!["/repo/with\0nul".to_owned()]),
            Err(WorkHistoryGitInputError::InvalidPath)
        );
        assert_eq!(
            admit_cwds(vec!["x".repeat(WORK_HISTORY_GIT_PATH_MAX + 1)]),
            Err(WorkHistoryGitInputError::PathTooLarge)
        );
    }

    #[test]
    fn git수집은_고정명령_timeout_output상한만_사용한다() {
        let fact = collect_git_fact_with("/private/repo", |cwd, args, timeout, output_max| {
            assert_eq!(cwd, Path::new("/private/repo"));
            assert_eq!(
                args,
                [
                    "status",
                    "--porcelain=v1",
                    "--branch",
                    "-z",
                    "--untracked-files=normal",
                ]
            );
            assert_eq!(timeout, WORK_HISTORY_GIT_TIMEOUT);
            assert_eq!(output_max, WORK_HISTORY_GIT_OUTPUT_MAX);
            Ok("## main\0 M src/main.rs\0".to_owned())
        })
        .expect("git fact");
        assert_eq!(fact.branch.as_deref(), Some("main"));
        assert_eq!(fact.changed_files, Some(1));
    }

    #[test]
    fn git실패와_non_repo는_fact를_남기지않는다() {
        let fact = collect_git_fact_with("/private/not-a-repo", |_, _, _, _| {
            Err(anyhow::anyhow!("git_command_failed"))
        });
        assert!(fact.is_none());
    }

    struct BlockingBackend {
        calls: Arc<AtomicUsize>,
        started: Option<mpsc::Sender<()>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl GitFactBackend for BlockingBackend {
        fn collect(&mut self, cwd: &str) -> Option<WorkHistoryGitFact> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                }
                let (lock, changed) = &*self.release;
                let mut released = lock.lock().expect("release lock");
                while !*released {
                    released = changed.wait(released).expect("release wait");
                }
            }
            Some(WorkHistoryGitFact {
                cwd: cwd.to_owned(),
                branch: Some("main".to_owned()),
                changed_files: Some(1),
            })
        }
    }

    #[test]
    fn 실행중_요청은_새_generation으로_교체되고_stale결과는_폐기된다() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (worker, input, receiver) = WorkHistoryGitWorker::spawn_with_backend(
            egui::Context::default(),
            BlockingBackend {
                calls: Arc::clone(&calls),
                started: Some(started_tx),
                release: Arc::clone(&release),
            },
        );

        input.publish(1, vec!["/slow".to_owned()]).unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first collection started");
        input.publish(2, vec!["/latest".to_owned()]).unwrap();
        {
            let (lock, changed) = &*release;
            *lock.lock().expect("release lock") = true;
            changed.notify_all();
        }

        let latest = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("latest result");
        assert_eq!(latest.generation, 2);
        assert_eq!(latest.facts.len(), 1);
        assert_eq!(latest.facts[0].cwd, "/latest");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(worker);
    }

    #[test]
    fn 결과_mailbox는_미소비_generation중_최신하나만_보유한다() {
        let mailbox = OutcomeMailbox::new();
        let receiver = WorkHistoryGitOutcomeReceiver {
            mailbox: Arc::clone(&mailbox),
            seen_sequence: AtomicU64::new(0),
        };
        for generation in 1..=32 {
            mailbox.invalidate(generation);
            assert!(mailbox.publish(
                generation,
                WorkHistoryGitOutcome {
                    generation,
                    facts: Vec::new().into(),
                },
            ));
        }

        let latest = receiver.try_recv().expect("latest result");
        assert_eq!(latest.generation, 32);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn fact와_snapshot_debug는_path와_branch를_노출하지않는다() {
        let fact = WorkHistoryGitFact {
            cwd: "/private/secret-repo".to_owned(),
            branch: Some("secret-branch".to_owned()),
            changed_files: Some(3),
        };
        let debug = format!("{fact:?}");
        assert!(!debug.contains("secret-repo"), "{debug}");
        assert!(!debug.contains("secret-branch"), "{debug}");

        let outcome = WorkHistoryGitOutcome {
            generation: 7,
            facts: vec![fact].into(),
        };
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("secret-repo"), "{debug}");
        assert!(!debug.contains("secret-branch"), "{debug}");
    }
}
