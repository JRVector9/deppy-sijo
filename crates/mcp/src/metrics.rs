//! Low-cost process-wide transport concurrency counters.
//!
//! Atomics allocate no thread and perform no polling. The counters include the
//! helper threads hidden inside the sync transports so resource tests do not
//! undercount a stdio connection as a single worker.

use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpTransportMetrics {
    pub active_threads: usize,
    pub peak_threads: usize,
    pub stdio_stdout_threads: usize,
    pub stdio_stderr_threads: usize,
    pub stdio_writer_threads: usize,
    pub http_sender_threads: usize,
    pub http_progress_threads: usize,
    pub active_http_send_permits: usize,
    pub peak_http_send_permits: usize,
    pub reaper_pending_http_senders: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum ThreadKind {
    StdioStdout,
    StdioStderr,
    StdioWriter,
    HttpSender,
    HttpProgress,
}

static ACTIVE_THREADS: AtomicUsize = AtomicUsize::new(0);
static PEAK_THREADS: AtomicUsize = AtomicUsize::new(0);
static STDIO_STDOUT: AtomicUsize = AtomicUsize::new(0);
static STDIO_STDERR: AtomicUsize = AtomicUsize::new(0);
static STDIO_WRITER: AtomicUsize = AtomicUsize::new(0);
static HTTP_SENDER: AtomicUsize = AtomicUsize::new(0);
static HTTP_PROGRESS: AtomicUsize = AtomicUsize::new(0);
static HTTP_SEND_PERMITS: AtomicUsize = AtomicUsize::new(0);
static PEAK_HTTP_SEND_PERMITS: AtomicUsize = AtomicUsize::new(0);
static REAPER_PENDING_HTTP_SENDERS: AtomicUsize = AtomicUsize::new(0);

pub fn transport_metrics() -> McpTransportMetrics {
    McpTransportMetrics {
        active_threads: ACTIVE_THREADS.load(Ordering::Relaxed),
        peak_threads: PEAK_THREADS.load(Ordering::Relaxed),
        stdio_stdout_threads: STDIO_STDOUT.load(Ordering::Relaxed),
        stdio_stderr_threads: STDIO_STDERR.load(Ordering::Relaxed),
        stdio_writer_threads: STDIO_WRITER.load(Ordering::Relaxed),
        http_sender_threads: HTTP_SENDER.load(Ordering::Relaxed),
        http_progress_threads: HTTP_PROGRESS.load(Ordering::Relaxed),
        active_http_send_permits: HTTP_SEND_PERMITS.load(Ordering::Relaxed),
        peak_http_send_permits: PEAK_HTTP_SEND_PERMITS.load(Ordering::Relaxed),
        reaper_pending_http_senders: REAPER_PENDING_HTTP_SENDERS.load(Ordering::Relaxed),
    }
}

pub(crate) struct ThreadGuard {
    kind: ThreadKind,
}

impl ThreadGuard {
    pub(crate) fn enter(kind: ThreadKind) -> Self {
        let active = ACTIVE_THREADS.fetch_add(1, Ordering::Relaxed) + 1;
        update_peak(&PEAK_THREADS, active);
        counter(kind).fetch_add(1, Ordering::Relaxed);
        Self { kind }
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        counter(self.kind).fetch_sub(1, Ordering::Relaxed);
        ACTIVE_THREADS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn http_send_permit_acquired() {
    let active = HTTP_SEND_PERMITS.fetch_add(1, Ordering::Relaxed) + 1;
    update_peak(&PEAK_HTTP_SEND_PERMITS, active);
}

pub(crate) fn http_send_permit_released() {
    HTTP_SEND_PERMITS.fetch_sub(1, Ordering::Relaxed);
}

pub(crate) fn set_reaper_pending_http_senders(pending: usize) {
    REAPER_PENDING_HTTP_SENDERS.store(pending, Ordering::Relaxed);
}

fn counter(kind: ThreadKind) -> &'static AtomicUsize {
    match kind {
        ThreadKind::StdioStdout => &STDIO_STDOUT,
        ThreadKind::StdioStderr => &STDIO_STDERR,
        ThreadKind::StdioWriter => &STDIO_WRITER,
        ThreadKind::HttpSender => &HTTP_SENDER,
        ThreadKind::HttpProgress => &HTTP_PROGRESS,
    }
}

fn update_peak(peak: &AtomicUsize, value: usize) {
    let mut observed = peak.load(Ordering::Relaxed);
    while value > observed {
        match peak.compare_exchange_weak(observed, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => observed = actual,
        }
    }
}
