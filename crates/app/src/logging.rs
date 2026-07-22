//! Bounded, secret-scanning application log sink.
//!
//! The sink owns no thread or timer. It is designed to be moved into exactly one
//! `tracing_appender` non-blocking worker by [`spawn_non_blocking_app_logger`]. Rotation and
//! directory maintenance happen only during construction, on a UTC day change observed while
//! writing a complete line, or when a byte ceiling would otherwise be crossed.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const APP_LOG_LINE_MAX_BYTES: usize = 64 * 1024;
pub const APP_LOG_QUEUE_LINES: usize = 1_024;
pub const APP_LOG_QUEUE_IS_LOSSY: bool = true;
pub const APP_LOG_ACTIVE_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const APP_LOG_TOTAL_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const APP_LOG_RETENTION_DAYS: i64 = 7;
pub const APP_LOG_MANAGED_FILE_PROBE_LIMIT: usize = 32;

const APP_LOG_DIRECTORY_ENTRY_PROBE_LIMIT: usize = 256;
const APP_LOG_PREFIX: &str = "app.log.";
const APP_LOG_NAME_BYTES: usize = APP_LOG_PREFIX.len() + 10;
const LINE_CONTENT_MAX_BYTES: usize = APP_LOG_LINE_MAX_BYTES - 1;
const SECRET_REPLACEMENT: &[u8] = b"app_log_event_replaced code=secret_like\n";
const OVERSIZED_REPLACEMENT: &[u8] = b"app_log_event_replaced code=line_too_large\n";
const PARTIAL_REPLACEMENT: &[u8] = b"app_log_event_replaced code=partial_line\n";

/// Static error taxonomy. No path, source line, or underlying I/O text is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppLogErrorCode {
    DirectoryUnavailable,
    DirectoryEntryLimit,
    ManagedCandidateLimit,
    ClockOutOfRange,
    ActiveFileUnsafe,
    ActiveFileUnavailable,
    MaintenanceUnavailable,
    WriteUnavailable,
    FlushUnavailable,
}

impl AppLogErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectoryUnavailable => "directory_unavailable",
            Self::DirectoryEntryLimit => "directory_entry_limit",
            Self::ManagedCandidateLimit => "managed_candidate_limit",
            Self::ClockOutOfRange => "clock_out_of_range",
            Self::ActiveFileUnsafe => "active_file_unsafe",
            Self::ActiveFileUnavailable => "active_file_unavailable",
            Self::MaintenanceUnavailable => "maintenance_unavailable",
            Self::WriteUnavailable => "write_unavailable",
            Self::FlushUnavailable => "flush_unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLogError {
    code: AppLogErrorCode,
}

impl AppLogError {
    const fn new(code: AppLogErrorCode) -> Self {
        Self { code }
    }

    pub const fn code(self) -> AppLogErrorCode {
        self.code
    }
}

impl std::fmt::Display for AppLogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code.as_str())
    }
}

impl std::error::Error for AppLogError {}

#[derive(Default)]
struct StatsInner {
    written_lines: AtomicU64,
    written_bytes: AtomicU64,
    redacted_lines: AtomicU64,
    oversized_lines: AtomicU64,
    partial_lines: AtomicU64,
    dropped_lines: AtomicU64,
    removed_files: AtomicU64,
    removed_bytes: AtomicU64,
    write_failures: AtomicU64,
    peak_buffered_bytes: AtomicU64,
}

/// Cloneable counters for diagnostics. The snapshot contains counts only.
#[derive(Clone, Default)]
pub struct AppLogStats {
    inner: Arc<StatsInner>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppLogStatsSnapshot {
    pub written_lines: u64,
    pub written_bytes: u64,
    pub redacted_lines: u64,
    pub oversized_lines: u64,
    pub partial_lines: u64,
    pub dropped_lines: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
    pub write_failures: u64,
    pub peak_buffered_bytes: u64,
}

impl AppLogStats {
    pub fn snapshot(&self) -> AppLogStatsSnapshot {
        AppLogStatsSnapshot {
            written_lines: self.inner.written_lines.load(Ordering::Relaxed),
            written_bytes: self.inner.written_bytes.load(Ordering::Relaxed),
            redacted_lines: self.inner.redacted_lines.load(Ordering::Relaxed),
            oversized_lines: self.inner.oversized_lines.load(Ordering::Relaxed),
            partial_lines: self.inner.partial_lines.load(Ordering::Relaxed),
            dropped_lines: self.inner.dropped_lines.load(Ordering::Relaxed),
            removed_files: self.inner.removed_files.load(Ordering::Relaxed),
            removed_bytes: self.inner.removed_bytes.load(Ordering::Relaxed),
            write_failures: self.inner.write_failures.load(Ordering::Relaxed),
            peak_buffered_bytes: self.inner.peak_buffered_bytes.load(Ordering::Relaxed),
        }
    }

    fn update_peak_buffered_bytes(&self, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let mut observed = self.inner.peak_buffered_bytes.load(Ordering::Relaxed);
        while bytes > observed {
            match self.inner.peak_buffered_bytes.compare_exchange_weak(
                observed,
                bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
    }
}

impl std::fmt::Debug for AppLogStats {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(formatter)
    }
}

/// Supplies a UTC Unix-day number. Implementations must not start a timer or polling worker.
pub trait UtcDayClock: Send + Sync + 'static {
    fn unix_day(&self) -> i64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemUtcDayClock;

impl UtcDayClock for SystemUtcDayClock {
    fn unix_day(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_secs() / 86_400).unwrap_or(i64::MAX),
            Err(error) => {
                let seconds = error.duration().as_secs();
                -i64::try_from(seconds.saturating_add(86_399) / 86_400).unwrap_or(i64::MAX)
            }
        }
    }
}

struct ManagedLog {
    day: i64,
    path: PathBuf,
    bytes: u64,
    removed: bool,
}

struct ManagedScan {
    regular: Vec<ManagedLog>,
    managed_names: usize,
}

/// Single-consumer bounded sink. Paths and source bytes are deliberately omitted from Debug.
pub struct AppLogSink {
    directory: PathBuf,
    clock: Arc<dyn UtcDayClock>,
    active_day: i64,
    active_path: PathBuf,
    active_file: File,
    active_bytes: u64,
    managed_total_bytes: u64,
    line: Vec<u8>,
    line_overflowed: bool,
    active_saturated: bool,
    total_saturated: bool,
    blocked_day: Option<i64>,
    stats: AppLogStats,
}

impl std::fmt::Debug for AppLogSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppLogSink")
            .field("active_bytes", &self.active_bytes)
            .field("managed_total_bytes", &self.managed_total_bytes)
            .field("buffered_bytes", &self.line.len())
            .field("line_overflowed", &self.line_overflowed)
            .field("active_saturated", &self.active_saturated)
            .field("total_saturated", &self.total_saturated)
            .field("stats", &self.stats)
            .finish()
    }
}

/// Open the production sink without spawning a worker.
pub fn open_app_log_sink(
    directory: impl AsRef<Path>,
) -> Result<(AppLogSink, AppLogStats), AppLogError> {
    open_app_log_sink_with_clock(directory, Arc::new(SystemUtcDayClock))
}

/// Open a sink with an injected non-polling clock. This is also the deterministic test seam.
pub fn open_app_log_sink_with_clock(
    directory: impl AsRef<Path>,
    clock: Arc<dyn UtcDayClock>,
) -> Result<(AppLogSink, AppLogStats), AppLogError> {
    let directory = directory.as_ref().to_path_buf();
    ensure_directory(&directory)?;
    let active_day = clock.unix_day();
    let active_name = managed_filename(active_day)?;
    let active_path = directory.join(active_name);

    let before = collect_managed_logs(&directory)?;
    let active_exists = before.regular.iter().any(|entry| entry.path == active_path);
    if before.managed_names >= APP_LOG_MANAGED_FILE_PROBE_LIMIT && !active_exists {
        return Err(AppLogError::new(AppLogErrorCode::ManagedCandidateLimit));
    }
    reject_unsafe_active_entry(&active_path)?;
    let mut active_file = open_active_file(&active_path)?;
    let active_bytes = active_file
        .metadata()
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?
        .len();
    let stats = AppLogStats::default();
    let active_bytes = sanitize_existing_active_file(&mut active_file, active_bytes, &stats)?;

    let mut sink = AppLogSink {
        directory,
        clock,
        active_day,
        active_path,
        active_file,
        active_bytes,
        managed_total_bytes: 0,
        line: Vec::with_capacity(1024),
        line_overflowed: false,
        active_saturated: active_bytes >= APP_LOG_ACTIVE_FILE_MAX_BYTES,
        total_saturated: false,
        blocked_day: None,
        stats: stats.clone(),
    };
    sink.maintain_directory()?;
    Ok((sink, stats))
}

/// Move the sink into exactly one bounded tracing-appender worker.
///
/// No worker is created until this explicit function is called.
pub fn spawn_non_blocking_app_logger(
    sink: AppLogSink,
) -> (
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
) {
    tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(APP_LOG_QUEUE_LINES)
        .lossy(APP_LOG_QUEUE_IS_LOSSY)
        .thread_name("app-log-writer")
        .finish(sink)
}

impl AppLogSink {
    fn emit_completed_line(&mut self, kind: CompletedLine) -> io::Result<()> {
        let day = self.clock.unix_day();
        if self.blocked_day == Some(day) {
            self.stats
                .inner
                .dropped_lines
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if day != self.active_day
            && let Err(error) = self.rotate(day)
        {
            self.blocked_day = Some(day);
            self.stats
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
            self.stats
                .inner
                .dropped_lines
                .fetch_add(1, Ordering::Relaxed);
            return Err(static_io_error(error.code()));
        }

        let output = match kind {
            CompletedLine::Complete => {
                let report = secret::scan_diagnostic_bytes(&self.line);
                if report.is_safe() {
                    OutputLine::Buffered
                } else {
                    self.stats
                        .inner
                        .redacted_lines
                        .fetch_add(1, Ordering::Relaxed);
                    OutputLine::Fixed(SECRET_REPLACEMENT)
                }
            }
            CompletedLine::Oversized => {
                self.stats
                    .inner
                    .oversized_lines
                    .fetch_add(1, Ordering::Relaxed);
                OutputLine::Fixed(OVERSIZED_REPLACEMENT)
            }
            CompletedLine::Partial => {
                self.stats
                    .inner
                    .partial_lines
                    .fetch_add(1, Ordering::Relaxed);
                OutputLine::Fixed(PARTIAL_REPLACEMENT)
            }
        };

        let bytes = u64::try_from(output.len(self.line.len())).unwrap_or(u64::MAX);
        if self.active_saturated
            || self.total_saturated
            || self.active_bytes.saturating_add(bytes) > APP_LOG_ACTIVE_FILE_MAX_BYTES
            || !self.ensure_total_room(bytes)?
        {
            if !self.active_saturated
                && self.active_bytes.saturating_add(bytes) > APP_LOG_ACTIVE_FILE_MAX_BYTES
            {
                self.maintain_directory().map_err(static_io_error_from)?;
                self.active_saturated = true;
            }
            self.stats
                .inner
                .dropped_lines
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let write_result = match output {
            OutputLine::Buffered => self.active_file.write_all(&self.line),
            OutputLine::Fixed(payload) => self.active_file.write_all(payload),
        };
        write_result.map_err(|_| {
            self.stats
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
            static_io_error(AppLogErrorCode::WriteUnavailable)
        })?;
        self.active_bytes = self.active_bytes.saturating_add(bytes);
        self.managed_total_bytes = self.managed_total_bytes.saturating_add(bytes);
        self.stats
            .inner
            .written_lines
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .inner
            .written_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    fn ensure_total_room(&mut self, bytes: u64) -> io::Result<bool> {
        if self.managed_total_bytes.saturating_add(bytes) <= APP_LOG_TOTAL_MAX_BYTES {
            return Ok(true);
        }
        self.maintain_directory_with_reserve(bytes)
            .map_err(static_io_error_from)?;
        if self.managed_total_bytes.saturating_add(bytes) <= APP_LOG_TOTAL_MAX_BYTES {
            return Ok(true);
        }
        self.total_saturated = true;
        Ok(false)
    }

    fn rotate(&mut self, day: i64) -> Result<(), AppLogError> {
        let active_name = managed_filename(day)?;
        let active_path = self.directory.join(active_name);
        let before = collect_managed_logs(&self.directory)?;
        let active_exists = before.regular.iter().any(|entry| entry.path == active_path);
        if before.managed_names >= APP_LOG_MANAGED_FILE_PROBE_LIMIT && !active_exists {
            return Err(AppLogError::new(AppLogErrorCode::ManagedCandidateLimit));
        }
        reject_unsafe_active_entry(&active_path)?;
        let mut active_file = open_active_file(&active_path)?;
        let active_bytes = active_file
            .metadata()
            .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?
            .len();
        let active_bytes =
            sanitize_existing_active_file(&mut active_file, active_bytes, &self.stats)?;
        self.active_file
            .flush()
            .map_err(|_| AppLogError::new(AppLogErrorCode::FlushUnavailable))?;
        self.active_day = day;
        self.active_path = active_path;
        self.active_file = active_file;
        self.active_bytes = active_bytes;
        self.active_saturated = active_bytes >= APP_LOG_ACTIVE_FILE_MAX_BYTES;
        self.total_saturated = false;
        self.blocked_day = None;
        self.maintain_directory()?;
        Ok(())
    }

    fn maintain_directory(&mut self) -> Result<(), AppLogError> {
        self.maintain_directory_with_reserve(0)
    }

    fn maintain_directory_with_reserve(&mut self, reserve_bytes: u64) -> Result<(), AppLogError> {
        let mut scan = collect_managed_logs(&self.directory)?;
        scan.regular.sort_by_key(|entry| entry.day);
        let minimum_day = self
            .active_day
            .saturating_sub(APP_LOG_RETENTION_DAYS.saturating_sub(1));

        for index in 0..scan.regular.len() {
            if scan.regular[index].day < minimum_day && scan.regular[index].path != self.active_path
            {
                self.remove_managed(&mut scan.regular[index])?;
            }
        }

        while retained_count(&scan.regular) > usize::try_from(APP_LOG_RETENTION_DAYS).unwrap_or(7) {
            let Some(index) = oldest_removable(&scan.regular, &self.active_path) else {
                break;
            };
            self.remove_managed(&mut scan.regular[index])?;
        }

        let mut total = retained_bytes(&scan.regular);
        while total.saturating_add(reserve_bytes) > APP_LOG_TOTAL_MAX_BYTES {
            let Some(index) = oldest_removable(&scan.regular, &self.active_path) else {
                break;
            };
            self.remove_managed(&mut scan.regular[index])?;
            total = retained_bytes(&scan.regular);
        }
        self.managed_total_bytes = total;
        Ok(())
    }

    fn remove_managed(&self, candidate: &mut ManagedLog) -> Result<(), AppLogError> {
        if candidate.removed || candidate.path == self.active_path {
            return Ok(());
        }
        match std::fs::symlink_metadata(&candidate.path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Err(AppLogError::new(AppLogErrorCode::MaintenanceUnavailable)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                candidate.removed = true;
                return Ok(());
            }
            Err(_) => return Err(AppLogError::new(AppLogErrorCode::MaintenanceUnavailable)),
        }
        match std::fs::remove_file(&candidate.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(AppLogError::new(AppLogErrorCode::MaintenanceUnavailable)),
        }
        candidate.removed = true;
        self.stats
            .inner
            .removed_files
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .inner
            .removed_bytes
            .fetch_add(candidate.bytes, Ordering::Relaxed);
        Ok(())
    }

    fn finish_partial_line(&mut self) -> io::Result<()> {
        if self.line.is_empty() && !self.line_overflowed {
            return Ok(());
        }
        let kind = if self.line_overflowed {
            CompletedLine::Oversized
        } else {
            CompletedLine::Partial
        };
        let result = self.emit_completed_line(kind);
        self.line.clear();
        self.line_overflowed = false;
        result
    }
}

impl Write for AppLogSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut remaining = buffer;
        while !remaining.is_empty() {
            let newline = remaining.iter().position(|byte| *byte == b'\n');
            let segment_end = newline.unwrap_or(remaining.len());
            let segment = &remaining[..segment_end];
            if !self.line_overflowed {
                let available = LINE_CONTENT_MAX_BYTES.saturating_sub(self.line.len());
                let retained = segment.len().min(available);
                self.line.extend_from_slice(&segment[..retained]);
                self.stats.update_peak_buffered_bytes(self.line.len());
                if retained < segment.len() {
                    self.line_overflowed = true;
                }
            }

            if newline.is_some() {
                let kind = if self.line_overflowed {
                    CompletedLine::Oversized
                } else {
                    self.line.push(b'\n');
                    self.stats.update_peak_buffered_bytes(self.line.len());
                    CompletedLine::Complete
                };
                self.emit_completed_line(kind)?;
                self.line.clear();
                self.line_overflowed = false;
                remaining = &remaining[segment_end + 1..];
            } else {
                break;
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.finish_partial_line()?;
        self.active_file.flush().map_err(|_| {
            self.stats
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
            static_io_error(AppLogErrorCode::FlushUnavailable)
        })
    }
}

impl Drop for AppLogSink {
    fn drop(&mut self) {
        if self.finish_partial_line().is_err() {
            self.stats
                .inner
                .write_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.active_file.flush();
    }
}

#[derive(Clone, Copy)]
enum CompletedLine {
    Complete,
    Oversized,
    Partial,
}

#[derive(Clone, Copy)]
enum OutputLine {
    Buffered,
    Fixed(&'static [u8]),
}

impl OutputLine {
    fn len(self, buffered: usize) -> usize {
        match self {
            Self::Buffered => buffered,
            Self::Fixed(payload) => payload.len(),
        }
    }
}

fn ensure_directory(directory: &Path) -> Result<(), AppLogError> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            return Ok(());
        }
        Ok(_) => return Err(AppLogError::new(AppLogErrorCode::DirectoryUnavailable)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(AppLogError::new(AppLogErrorCode::DirectoryUnavailable)),
    }
    std::fs::create_dir_all(directory)
        .map_err(|_| AppLogError::new(AppLogErrorCode::DirectoryUnavailable))?;
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|_| AppLogError::new(AppLogErrorCode::DirectoryUnavailable))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(AppLogError::new(AppLogErrorCode::DirectoryUnavailable));
    }
    Ok(())
}

fn reject_unsafe_active_entry(path: &Path) -> Result<(), AppLogError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(AppLogError::new(AppLogErrorCode::ActiveFileUnsafe)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(AppLogError::new(AppLogErrorCode::ActiveFileUnavailable)),
    }
}

fn open_active_file(path: &Path) -> Result<File, AppLogError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    configure_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;
    let metadata = file
        .metadata()
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;
    if !metadata.is_file() {
        return Err(AppLogError::new(AppLogErrorCode::ActiveFileUnsafe));
    }
    Ok(file)
}

fn sanitize_existing_active_file(
    file: &mut File,
    original_bytes: u64,
    stats: &AppLogStats,
) -> Result<u64, AppLogError> {
    if original_bytes == 0 {
        return Ok(0);
    }
    if original_bytes > APP_LOG_ACTIVE_FILE_MAX_BYTES {
        truncate_active_file(file, original_bytes, 0, stats)?;
        return Ok(0);
    }

    let length = usize::try_from(original_bytes)
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;
    let mut bytes = vec![0u8; length];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;

    if !secret::scan_diagnostic_bytes(&bytes).is_safe() {
        truncate_active_file(file, original_bytes, 0, stats)?;
        return Ok(0);
    }

    let complete_bytes = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let complete_bytes = u64::try_from(complete_bytes).unwrap_or(0);
    if complete_bytes < original_bytes {
        truncate_active_file(file, original_bytes, complete_bytes, stats)?;
    }
    Ok(complete_bytes)
}

fn truncate_active_file(
    file: &mut File,
    original_bytes: u64,
    retained_bytes: u64,
    stats: &AppLogStats,
) -> Result<(), AppLogError> {
    file.set_len(retained_bytes)
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;
    file.seek(SeekFrom::End(0))
        .map_err(|_| AppLogError::new(AppLogErrorCode::ActiveFileUnavailable))?;
    stats.inner.removed_bytes.fetch_add(
        original_bytes.saturating_sub(retained_bytes),
        Ordering::Relaxed,
    );
    Ok(())
}

#[allow(unused_variables)]
fn configure_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        #[cfg(target_os = "macos")]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0o400000);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn collect_managed_logs(directory: &Path) -> Result<ManagedScan, AppLogError> {
    let entries = std::fs::read_dir(directory)
        .map_err(|_| AppLogError::new(AppLogErrorCode::DirectoryUnavailable))?;
    let mut regular = Vec::new();
    let mut managed_names = 0usize;
    for (entry_index, entry) in entries.enumerate() {
        if entry_index >= APP_LOG_DIRECTORY_ENTRY_PROBE_LIMIT {
            return Err(AppLogError::new(AppLogErrorCode::DirectoryEntryLimit));
        }
        let entry = entry.map_err(|_| AppLogError::new(AppLogErrorCode::DirectoryUnavailable))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(day) = parse_managed_filename(&name) else {
            continue;
        };
        managed_names += 1;
        if managed_names > APP_LOG_MANAGED_FILE_PROBE_LIMIT {
            return Err(AppLogError::new(AppLogErrorCode::ManagedCandidateLimit));
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|_| AppLogError::new(AppLogErrorCode::MaintenanceUnavailable))?;
        if metadata.file_type().is_file() {
            regular.push(ManagedLog {
                day,
                path,
                bytes: metadata.len(),
                removed: false,
            });
        }
    }
    Ok(ManagedScan {
        regular,
        managed_names,
    })
}

fn retained_count(entries: &[ManagedLog]) -> usize {
    entries.iter().filter(|entry| !entry.removed).count()
}

fn retained_bytes(entries: &[ManagedLog]) -> u64 {
    entries
        .iter()
        .filter(|entry| !entry.removed)
        .fold(0u64, |total, entry| total.saturating_add(entry.bytes))
}

fn oldest_removable(entries: &[ManagedLog], active_path: &Path) -> Option<usize> {
    entries
        .iter()
        .position(|entry| !entry.removed && entry.path != active_path)
}

fn static_io_error(code: AppLogErrorCode) -> io::Error {
    io::Error::other(code.as_str())
}

fn static_io_error_from(error: AppLogError) -> io::Error {
    static_io_error(error.code())
}

fn managed_filename(day: i64) -> Result<String, AppLogError> {
    let (year, month, date) = civil_from_days(day);
    if !(0..=9999).contains(&year) {
        return Err(AppLogError::new(AppLogErrorCode::ClockOutOfRange));
    }
    Ok(format!("{APP_LOG_PREFIX}{year:04}-{month:02}-{date:02}"))
}

fn parse_managed_filename(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    if bytes.len() != APP_LOG_NAME_BYTES
        || !bytes.starts_with(APP_LOG_PREFIX.as_bytes())
        || bytes[12] != b'-'
        || bytes[15] != b'-'
    {
        return None;
    }
    let year = parse_decimal(&bytes[8..12])?;
    let month = parse_decimal(&bytes[13..15])?;
    let date = parse_decimal(&bytes[16..18])?;
    let day = days_from_civil(year, month, date)?;
    (civil_from_days(day) == (year, month, date)).then_some(day)
}

fn parse_decimal(bytes: &[u8]) -> Option<i32> {
    bytes.iter().try_fold(0i32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + i32::from(byte - b'0'))
    })
}

fn civil_from_days(day: i64) -> (i32, i32, i32) {
    let shifted = day.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (
        i32::try_from(year).unwrap_or(if year.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        }),
        i32::try_from(month).unwrap_or_default(),
        i32::try_from(date).unwrap_or_default(),
    )
}

fn days_from_civil(mut year: i32, month: i32, date: i32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&date) {
        return None;
    }
    year -= i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + date - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize};
    use std::time::Duration;

    struct FakeClock {
        day: AtomicI64,
        calls: AtomicUsize,
    }

    impl FakeClock {
        fn new(day: i64) -> Self {
            Self {
                day: AtomicI64::new(day),
                calls: AtomicUsize::new(0),
            }
        }

        fn set(&self, day: i64) {
            self.day.store(day, Ordering::SeqCst);
        }
    }

    impl UtcDayClock for FakeClock {
        fn unix_day(&self) -> i64 {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.day.load(Ordering::SeqCst)
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "deppy-app-log-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn open_fake(root: &Path, clock: Arc<FakeClock>) -> (AppLogSink, AppLogStats) {
        open_app_log_sink_with_clock(root, clock).unwrap()
    }

    fn managed_path(root: &Path, day: i64) -> PathBuf {
        root.join(managed_filename(day).unwrap())
    }

    fn sparse_managed_file(root: &Path, day: i64, bytes: u64) {
        let mut file = File::create(managed_path(root, day)).unwrap();
        file.set_len(bytes).unwrap();
        if bytes > 0 {
            file.seek(SeekFrom::Start(bytes - 1)).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn utc_filename_grammar_round_trips_and_rejects_unmanaged_names() {
        assert_eq!(managed_filename(0).unwrap(), "app.log.1970-01-01");
        assert_eq!(parse_managed_filename("app.log.1970-01-01"), Some(0));
        for day in [-719_162, 0, 20_000, 2_932_896] {
            let name = managed_filename(day).unwrap();
            assert_eq!(parse_managed_filename(&name), Some(day));
        }
        for name in [
            "app.log",
            "app.log.2026-7-22",
            "app.log.2026-02-30",
            "app.log.2026-07-22.extra",
            "other.log.2026-07-22",
        ] {
            assert_eq!(parse_managed_filename(name), None, "name={name}");
        }
    }

    #[test]
    fn complete_line_boundary_is_exact_and_plus_one_is_replaced() {
        let root = temp_root("line-boundary");
        let clock = Arc::new(FakeClock::new(20_000));
        let (mut sink, stats) = open_fake(&root, clock);
        let exact = vec![b'a'; LINE_CONTENT_MAX_BYTES];
        sink.write_all(&exact).unwrap();
        sink.write_all(b"\n").unwrap();
        let plus_one = vec![b'b'; LINE_CONTENT_MAX_BYTES + 1];
        sink.write_all(&plus_one).unwrap();
        sink.write_all(b"\n").unwrap();
        sink.flush().unwrap();

        let bytes = std::fs::read(managed_path(&root, 20_000)).unwrap();
        assert_eq!(&bytes[..exact.len()], exact.as_slice());
        assert_eq!(bytes[exact.len()], b'\n');
        assert_eq!(&bytes[APP_LOG_LINE_MAX_BYTES..], OVERSIZED_REPLACEMENT);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.oversized_lines, 1);
        assert_eq!(
            snapshot.peak_buffered_bytes as usize,
            APP_LOG_LINE_MAX_BYTES
        );
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_and_partial_lines_never_write_source_markers() {
        const SECRET_MARKER: &str = "unique-app-log-secret-marker-7391";
        const PARTIAL_MARKER: &str = "unique-partial-source-marker-9137";
        let root = temp_root("replacement");
        let clock = Arc::new(FakeClock::new(20_001));
        let (mut sink, stats) = open_fake(&root, clock);
        writeln!(sink, "Authorization: Bearer {SECRET_MARKER}").unwrap();
        sink.write_all(PARTIAL_MARKER.as_bytes()).unwrap();
        sink.flush().unwrap();

        let bytes = std::fs::read(managed_path(&root, 20_001)).unwrap();
        assert!(
            !bytes
                .windows(SECRET_MARKER.len())
                .any(|part| part == SECRET_MARKER.as_bytes())
        );
        assert!(
            !bytes
                .windows(PARTIAL_MARKER.len())
                .any(|part| part == PARTIAL_MARKER.as_bytes())
        );
        assert_eq!(bytes, [SECRET_REPLACEMENT, PARTIAL_REPLACEMENT].concat());
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.redacted_lines, 1);
        assert_eq!(snapshot.partial_lines, 1);
        let debug = format!("{sink:?} {stats:?}");
        assert!(!debug.contains(SECRET_MARKER));
        assert!(!debug.contains(PARTIAL_MARKER));
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_partial_buffer_is_bounded_and_replaced_once_on_flush() {
        let root = temp_root("oversized-partial");
        let clock = Arc::new(FakeClock::new(20_002));
        let (mut sink, stats) = open_fake(&root, clock);
        sink.write_all(&vec![b'z'; APP_LOG_LINE_MAX_BYTES * 4])
            .unwrap();
        assert_eq!(sink.line.len(), LINE_CONTENT_MAX_BYTES);
        assert!(sink.line_overflowed);
        sink.flush().unwrap();
        sink.flush().unwrap();
        assert_eq!(
            std::fs::read(managed_path(&root, 20_002)).unwrap(),
            OVERSIZED_REPLACEMENT
        );
        assert_eq!(stats.snapshot().oversized_lines, 1);
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_file_accepts_exact_byte_ceiling_and_drops_plus_one_line() {
        let root = temp_root("active-cap");
        let day = 20_003;
        sparse_managed_file(&root, day, APP_LOG_ACTIVE_FILE_MAX_BYTES - 2);
        let clock = Arc::new(FakeClock::new(day));
        let (mut sink, stats) = open_fake(&root, clock);
        sink.write_all(b"x\n").unwrap();
        sink.write_all(b"y\n").unwrap();
        sink.flush().unwrap();
        assert_eq!(
            std::fs::metadata(managed_path(&root, day)).unwrap().len(),
            APP_LOG_ACTIVE_FILE_MAX_BYTES
        );
        assert_eq!(stats.snapshot().dropped_lines, 1);
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_active_file_is_scanned_and_incomplete_tail_is_discarded() {
        const SECRET_MARKER: &str = "existing-secret-marker-1907";
        const PARTIAL_MARKER: &str = "existing-partial-marker-4812";
        let day = 20_004;

        let secret_root = temp_root("existing-secret");
        let secret_source = format!("Authorization: Bearer {SECRET_MARKER}\n");
        std::fs::write(managed_path(&secret_root, day), &secret_source).unwrap();
        let (secret_sink, secret_stats) = open_fake(&secret_root, Arc::new(FakeClock::new(day)));
        assert_eq!(std::fs::read(managed_path(&secret_root, day)).unwrap(), b"");
        assert_eq!(
            secret_stats.snapshot().removed_bytes,
            secret_source.len() as u64
        );
        drop(secret_sink);
        std::fs::remove_dir_all(secret_root).unwrap();

        let partial_root = temp_root("existing-partial");
        let partial_source = format!("complete\n{PARTIAL_MARKER}");
        std::fs::write(managed_path(&partial_root, day), &partial_source).unwrap();
        let (partial_sink, partial_stats) = open_fake(&partial_root, Arc::new(FakeClock::new(day)));
        assert_eq!(
            std::fs::read(managed_path(&partial_root, day)).unwrap(),
            b"complete\n"
        );
        assert_eq!(
            partial_stats.snapshot().removed_bytes,
            PARTIAL_MARKER.len() as u64
        );
        drop(partial_sink);
        std::fs::remove_dir_all(partial_root).unwrap();
    }

    #[test]
    fn existing_active_file_over_byte_ceiling_is_truncated_without_scanning() {
        let root = temp_root("existing-over-cap");
        let day = 20_005;
        sparse_managed_file(&root, day, APP_LOG_ACTIVE_FILE_MAX_BYTES + 1);
        let (sink, stats) = open_fake(&root, Arc::new(FakeClock::new(day)));
        assert_eq!(
            std::fs::metadata(managed_path(&root, day)).unwrap().len(),
            0
        );
        assert_eq!(
            stats.snapshot().removed_bytes,
            APP_LOG_ACTIVE_FILE_MAX_BYTES + 1
        );
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exact_total_ceiling_evicts_oldest_before_next_line() {
        let root = temp_root("total-reserve");
        let current = 20_010;
        for day in current - 4..current {
            sparse_managed_file(&root, day, APP_LOG_ACTIVE_FILE_MAX_BYTES);
        }
        let clock = Arc::new(FakeClock::new(current));
        let (mut sink, stats) = open_fake(&root, clock);
        assert_eq!(sink.managed_total_bytes, APP_LOG_TOTAL_MAX_BYTES);

        sink.write_all(b"x\n").unwrap();
        sink.flush().unwrap();

        assert!(!managed_path(&root, current - 4).exists());
        assert_eq!(std::fs::read(managed_path(&root, current)).unwrap(), b"x\n");
        assert_eq!(stats.snapshot().removed_files, 1);
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_enforces_day_count_age_and_total_bytes_oldest_first() {
        let root = temp_root("gc");
        let current = 20_020;
        for day in current - 8..=current {
            sparse_managed_file(&root, day, APP_LOG_ACTIVE_FILE_MAX_BYTES);
        }
        let clock = Arc::new(FakeClock::new(current));
        let (sink, stats) = open_fake(&root, clock);

        assert!(managed_path(&root, current).exists());
        assert!(!managed_path(&root, current - 8).exists());
        assert!(!managed_path(&root, current - 7).exists());
        let scan = collect_managed_logs(&root).unwrap();
        assert!(scan.regular.len() <= APP_LOG_RETENTION_DAYS as usize);
        assert!(retained_bytes(&scan.regular) <= APP_LOG_TOTAL_MAX_BYTES);
        assert_eq!(retained_bytes(&scan.regular), APP_LOG_TOTAL_MAX_BYTES);
        assert_eq!(stats.snapshot().removed_files, 5);
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn utc_day_change_rotates_on_next_complete_line_without_polling() {
        let root = temp_root("rollover");
        let clock = Arc::new(FakeClock::new(20_030));
        let (mut sink, _) = open_fake(&root, Arc::clone(&clock));
        sink.write_all(b"before\n").unwrap();
        clock.set(20_031);
        sink.write_all(b"after\n").unwrap();
        sink.flush().unwrap();
        assert_eq!(
            std::fs::read(managed_path(&root, 20_030)).unwrap(),
            b"before\n"
        );
        assert_eq!(
            std::fs::read(managed_path(&root, 20_031)).unwrap(),
            b"after\n"
        );
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_candidate_limit_plus_one_fails_closed_without_opening_current() {
        let root = temp_root("candidate-flood");
        let current = 21_000;
        for day in current - APP_LOG_MANAGED_FILE_PROBE_LIMIT as i64..current {
            sparse_managed_file(&root, day, 1);
        }
        let error =
            open_app_log_sink_with_clock(&root, Arc::new(FakeClock::new(current))).unwrap_err();
        assert_eq!(error.code(), AppLogErrorCode::ManagedCandidateLimit);
        assert!(!managed_path(&root, current).exists());
        let debug = format!("{error:?}");
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_symlink_is_never_followed_or_deleted() {
        use std::os::unix::fs::symlink;

        let root = temp_root("symlink");
        let outside = root.with_extension("outside");
        std::fs::write(&outside, b"outside-marker").unwrap();
        let link_day = 20_040;
        let link = managed_path(&root, link_day);
        symlink(&outside, &link).unwrap();

        let (sink, _) = open_fake(&root, Arc::new(FakeClock::new(link_day + 10)));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside-marker");
        drop(sink);

        let error =
            open_app_log_sink_with_clock(&root, Arc::new(FakeClock::new(link_day))).unwrap_err();
        assert_eq!(error.code(), AppLogErrorCode::ActiveFileUnsafe);
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside-marker");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn construction_has_no_background_clock_or_file_activity() {
        let root = temp_root("idle");
        let day = 20_050;
        let clock = Arc::new(FakeClock::new(day));
        let (sink, stats) = open_fake(&root, Arc::clone(&clock));
        let calls = clock.calls.load(Ordering::SeqCst);
        let modified = std::fs::metadata(managed_path(&root, day))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(clock.calls.load(Ordering::SeqCst), calls);
        assert_eq!(
            std::fs::metadata(managed_path(&root, day))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        assert_eq!(stats.snapshot(), AppLogStatsSnapshot::default());
        drop(sink);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queue_policy_and_explicit_single_worker_constructor_are_fixed() {
        assert_eq!(APP_LOG_QUEUE_LINES, 1_024);
        const { assert!(APP_LOG_QUEUE_IS_LOSSY) };
        let _constructor: fn(
            AppLogSink,
        ) -> (
            tracing_appender::non_blocking::NonBlocking,
            tracing_appender::non_blocking::WorkerGuard,
        ) = spawn_non_blocking_app_logger;
    }
}
