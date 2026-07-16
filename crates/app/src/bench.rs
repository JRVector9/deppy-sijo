//! 렌더러 A/B 실측 인프라 (B1). **전부 env 게이트 뒤** — 미설정이면 이 모듈의 코드는
//! `Bench::from_env() == None`에서 끝나고 기존 실행 경로·idle repaint 0 원칙이 그대로다.
//!
//! 목적: Glow vs Wgpu(Metal) 경로를 동일 조건에서 실측해 PR-04/05/06을 데이터로 판정한다.
//! 렌더 경로 자체는 바꾸지 않는다 (egui/epaint + dirty-row galley cache 유지).
//!
//! ## 환경변수
//! - `DEPPY_RENDERER=glow|wgpu` — `render-glow` 벤치 빌드의 렌더러 선택 (main.rs)
//! - `DEPPY_RENDER_BENCH=1` — 벤치 모드(시나리오 드라이버 + JSONL)
//! - `DEPPY_BENCH_OUT=<path>` — JSONL 경로 (미지정 시 stderr)
//! - `DEPPY_BENCH_SCENARIO=idle|dirty1|bulk|fullscreen|switch|createdelete`
//! - `DEPPY_BENCH_WORKSPACES=N` / `DEPPY_BENCH_SECS=N` / `DEPPY_BENCH_ITERS=N`
//! - `DEPPY_RESOURCE_STATS=1` — 2초 주기 rss 이벤트 (벤치 없이 단독 사용 가능)
//! - `DEPPY_ALLOC_STATS=1` — 프레임 alloc 카운터 (cargo feature `bench-alloc` 빌드에서만)

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// alloc 카운터 (cargo feature `bench-alloc` 전용 — 기본 빌드에 코드 자체가 없다)
// ---------------------------------------------------------------------------

/// 스레드별 alloc 카운터. **프로세스 전역이 아니라 thread-local**이라 UI 스레드에서 읽으면
/// 렌더 루프의 alloc만 잡힌다 (터미널 워커 스레드의 스냅샷 alloc은 섞이지 않는다 — §3-B
/// "steady-state 렌더 루프 heap allocation" 목표에 맞춘 선택).
#[cfg(feature = "bench-alloc")]
mod counting_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }

    /// Cell<u64> + const 초기화라 TLS 등록이 힙을 쓰지 않는다(재귀 alloc 없음). 스레드 소멸
    /// 중 TLS 접근 불가 구간에서는 try_with가 Err → 조용히 건너뛴다.
    fn bump(size: usize) {
        let _ = COUNT.try_with(|c| c.set(c.get().wrapping_add(1)));
        let _ = BYTES.try_with(|b| b.set(b.get().wrapping_add(size as u64)));
    }

    pub fn thread_stats() -> (u64, u64) {
        let count = COUNT.try_with(|c| c.get()).unwrap_or(0);
        let bytes = BYTES.try_with(|b| b.get()).unwrap_or(0);
        (count, bytes)
    }

    pub struct Counting;

    // SAFETY: 모든 경로를 System에 위임하고 카운터만 더한다.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            bump(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            bump(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[cfg(feature = "bench-alloc")]
#[global_allocator]
static GLOBAL_ALLOC: counting_alloc::Counting = counting_alloc::Counting;

/// 현재 스레드의 (alloc 횟수, alloc 바이트). feature가 없으면 None — **0을 지어내지 않는다**.
fn thread_alloc_stats() -> Option<(u64, u64)> {
    #[cfg(feature = "bench-alloc")]
    {
        Some(counting_alloc::thread_stats())
    }
    #[cfg(not(feature = "bench-alloc"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// 옵션
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scenario {
    Idle,
    Dirty1,
    Bulk,
    Fullscreen,
    Switch,
    CreateDelete,
    /// 에이전트 TUI 근사 — 실제 워크로드(Claude/Codex)의 렌더 구조를 재현한다
    /// (2026-07-14): alt screen + 스피너(10Hz) + 부분 갱신 + 스트리밍 출력 + 색·박스.
    /// 합성 벤치(yes 대량 출력)가 대표하지 못하는 이 앱의 실제 핫패스다.
    AgentTui,
}

impl Scenario {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "idle" => Self::Idle,
            "dirty1" => Self::Dirty1,
            "bulk" => Self::Bulk,
            "fullscreen" => Self::Fullscreen,
            "switch" => Self::Switch,
            "createdelete" => Self::CreateDelete,
            "agenttui" => Self::AgentTui,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dirty1 => "dirty1",
            Self::Bulk => "bulk",
            Self::Fullscreen => "fullscreen",
            Self::Switch => "switch",
            Self::CreateDelete => "createdelete",
            Self::AgentTui => "agenttui",
        }
    }

    /// 시나리오가 활성 워크스페이스에서 돌릴 셸 명령. idle은 출력이 0이어야 하므로
    /// 프롬프트조차 찍지 않는 sleep을 쓴다 (idle repaint 0 검증이 목적).
    pub fn command(self) -> Option<(String, Vec<String>)> {
        let script = match self {
            Self::Idle => "exec sleep 86400",
            // 한 행만 빠르게 갱신 — \r로 커서를 되감아 같은 행을 덮어쓴다
            Self::Dirty1 => "i=0; while :; do printf '\\rprogress=%d' $i; i=$((i+1)); done",
            // 대량 로그 (ASCII + 한글 혼합)
            Self::Bulk => {
                "yes 'The quick brown fox 한글 테스트 1234567890' | head -n 200000; sleep 86400"
            }
            // clear + 전체 viewport 채우기 반복
            Self::Fullscreen => {
                "line='The quick brown fox 한글 테스트 1234567890'; \
                 while :; do clear; i=0; \
                 while [ $i -lt 45 ]; do printf '%02d %s\\n' $i \"$line\"; i=$((i+1)); done; done"
            }
            // switch/createdelete는 드라이버가 워크스페이스를 조작한다 — 셸은 조용한 것으로.
            Self::Switch | Self::CreateDelete => "exec sleep 86400",
            // 에이전트 TUI 근사: alt screen 진입 → 상단 박스 + 스피너(10Hz, 커서 이동으로
            // 한 셀만 갱신) + 하단에 스트리밍 텍스트(0.5s마다 한 줄) + 8색 사용.
            // 실제 Claude/Codex TUI의 렌더 구조(부분 갱신 + 애니메이션 + 스크롤)를 흉내낸다.
            Self::AgentTui => {
                "printf '\\033[?1049h\\033[2J'; \
                 printf '\\033[1;1H\\033[36m╭──────────────────────────────╮\\033[0m'; \
                 printf '\\033[2;1H\\033[36m│\\033[0m \\033[1mdeppy agent\\033[0m  status:      \\033[36m│\\033[0m'; \
                 printf '\\033[3;1H\\033[36m╰──────────────────────────────╯\\033[0m'; \
                 i=0; row=5; \
                 while :; do \
                   for f in '|' '/' '-' '\\\\'; do \
                     printf '\\033[2;28H\\033[33m%s\\033[0m' \"$f\"; \
                     sleep 0.1; \
                   done; \
                   i=$((i+1)); \
                   printf '\\033[%d;1H\\033[32m▸\\033[0m tool call %03d — 한글 출력 테스트 \\033[2mdim\\033[0m\\033[K' $row $i; \
                   row=$((row+1)); \
                   [ $row -gt 40 ] && { printf '\\033[5;1H\\033[J'; row=5; }; \
                 done"
            }
        };
        Some((
            "/bin/sh".to_owned(),
            vec!["-c".to_owned(), script.to_owned()],
        ))
    }
}

#[derive(Clone, Debug)]
pub struct BenchOptions {
    /// DEPPY_RENDER_BENCH=1 — 시나리오 드라이버 활성
    pub driver: bool,
    pub scenario: Scenario,
    pub workspaces: usize,
    pub secs: u64,
    pub iters: usize,
    pub resource_stats: bool,
    pub alloc_stats: bool,
    pub out: Option<std::path::PathBuf>,
}

impl BenchOptions {
    fn from_env() -> Option<Self> {
        let driver = env_flag("DEPPY_RENDER_BENCH");
        let resource_stats = env_flag("DEPPY_RESOURCE_STATS");
        if !driver && !resource_stats {
            return None;
        }
        let scenario = std::env::var("DEPPY_BENCH_SCENARIO")
            .ok()
            .and_then(|value| Scenario::parse(&value))
            .unwrap_or(Scenario::Idle);
        Some(Self {
            driver,
            scenario,
            workspaces: env_usize("DEPPY_BENCH_WORKSPACES", 1).max(1),
            secs: env_usize("DEPPY_BENCH_SECS", 30) as u64,
            iters: env_usize("DEPPY_BENCH_ITERS", 100).max(1),
            resource_stats,
            // feature 없이 DEPPY_ALLOC_STATS만 켜면 alloc 필드는 null로 남는다(0 위조 금지).
            alloc_stats: env_flag("DEPPY_ALLOC_STATS") && thread_alloc_stats().is_some(),
            out: std::env::var_os("DEPPY_BENCH_OUT").map(std::path::PathBuf::from),
        })
    }
}

fn env_flag(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| value != "0" && !value.is_empty())
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// 벤치 모드(=사용자 데이터와 격리된 임시 data dir을 써야 하는 모드)인지.
/// `DEPPY_RESOURCE_STATS`만 켠 경우는 실제 앱을 관찰하는 용도라 격리하지 않는다.
pub fn isolate_data_dir() -> bool {
    env_flag("DEPPY_RENDER_BENCH")
}

// ---------------------------------------------------------------------------
// JSONL 로그
// ---------------------------------------------------------------------------

enum Sink {
    File(std::io::BufWriter<std::fs::File>),
    Stderr,
}

pub struct BenchLog {
    sink: std::sync::Mutex<Sink>,
}

impl BenchLog {
    fn new(out: Option<&std::path::Path>) -> Self {
        let sink = out
            .and_then(|path| match std::fs::File::create(path) {
                Ok(file) => Some(Sink::File(std::io::BufWriter::new(file))),
                Err(e) => {
                    eprintln!(
                        "bench: JSONL 파일 생성 실패({}) — stderr로 대체: {e}",
                        path.display()
                    );
                    None
                }
            })
            .unwrap_or(Sink::Stderr);
        Self {
            sink: std::sync::Mutex::new(sink),
        }
    }

    /// 이벤트 1건 = 한 줄. `t`(unix ms)와 `ev`는 여기서 붙인다.
    pub fn emit(&self, ev: &str, mut fields: serde_json::Map<String, serde_json::Value>) {
        let mut line = serde_json::Map::new();
        line.insert("t".to_owned(), unix_ms().into());
        line.insert("ev".to_owned(), ev.into());
        line.append(&mut fields);
        let Ok(text) = serde_json::to_string(&serde_json::Value::Object(line)) else {
            return;
        };
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        let _ = match &mut *sink {
            Sink::File(file) => writeln!(file, "{text}").and_then(|()| file.flush()),
            Sink::Stderr => writeln!(std::io::stderr(), "{text}"),
        };
    }

    /// eframe이 아직 생성되지 않은 시작 구간에서도 같은 RSS 이벤트 형식을 쓴다.
    /// 샌드박스에서 GUI 초기화가 막혀도 config/DB/복구까지의 회귀를 기준 빌드와 비교할 수 있다.
    pub fn emit_rss_stage(&self, stage: &str, workspaces: usize) {
        emit_rss(self, stage, sample_rss(), workspaces);
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

macro_rules! fields {
    ($($key:literal : $value:expr),* $(,)?) => {{
        let mut map = serde_json::Map::new();
        $( map.insert($key.to_owned(), serde_json::json!($value)); )*
        map
    }};
}

// ---------------------------------------------------------------------------
// RSS / 스레드 샘플링
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RssSample {
    pub app_bytes: u64,
    pub child_bytes: u64,
    pub threads: Option<usize>,
}

/// 앱 RSS + 자식 프로세스 트리 RSS를 한 번의 `ps`로 분리 측정한다.
///
/// runtime::resource_monitor가 같은 기법(ps -axo + ppid 트리 워크)을 쓰지만, 그쪽의 트리
/// 워크는 private이고 공개 API는 세션(ProcessIdentity) 단위라 "앱의 모든 자식"을 못 준다.
/// 그래서 기법만 동일하게 재현했다 — 결과는 같은 `ps` 소스다.
pub fn sample_rss() -> RssSample {
    let me = std::process::id();
    // macOS의 sandbox/TCC 환경에서는 `ps`가 자신의 프로세스조차 열거하지 못할 수 있다.
    // 자기 RSS는 커널의 proc_pidinfo로 직접 읽으면 subprocess도, 전체 프로세스 목록 권한도
    // 필요 없다. 다른 Unix와 proc_pidinfo 실패 시에만 아래 `ps` 행을 fallback으로 쓴다.
    let mut app_bytes = own_rss_bytes().unwrap_or(0);
    let mut child_bytes = 0u64;

    // 측정 도구인 `ps` 자신도 우리 자식으로 잡힌다 — 제외하지 않으면 child_bytes에
    // 상수 오차(≈1.7MB)가 섞인다 (첫 실측에서 발견).
    let (rows, ps_pid) = process_rows();
    // 내 pid의 자손 전체를 BFS로 모은다 (PTY 셸 + 그 손자들 + mcp-proxy 등).
    let mut descendants: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (pid, ppid, _) in &rows {
            if Some(*pid) == ps_pid {
                continue;
            }
            if *ppid == me || descendants.contains(ppid) {
                changed |= descendants.insert(*pid);
            }
        }
    }
    for (pid, _, rss) in &rows {
        if *pid == me {
            app_bytes = *rss;
        } else if descendants.contains(pid) {
            child_bytes = child_bytes.saturating_add(*rss);
        }
    }

    RssSample {
        app_bytes,
        child_bytes,
        threads: thread_count(),
    }
}

#[cfg(target_os = "macos")]
fn own_task_info() -> Option<libc::proc_taskinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: 현재 프로세스 pid와 정확한 크기의 쓰기 가능한 버퍼를 넘긴다.
    let rc = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (rc == size).then(|| {
        // SAFETY: proc_pidinfo가 구조체 전체(size 바이트)를 채웠다.
        unsafe { info.assume_init() }
    })
}

#[cfg(target_os = "macos")]
fn own_rss_bytes() -> Option<u64> {
    own_task_info().map(|info| info.pti_resident_size)
}

#[cfg(not(target_os = "macos"))]
fn own_rss_bytes() -> Option<u64> {
    None
}

/// (pid, ppid, rss_bytes) 목록과 **우리가 띄운 `ps` 자신의 pid**(자기 제외용).
fn process_rows() -> (Vec<(u32, u32, u64)>, Option<u32>) {
    let child = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .stdout(std::process::Stdio::piped())
        .spawn();
    let Ok(child) = child else {
        return (Vec::new(), None);
    };
    let ps_pid = child.id();
    let Ok(output) = child.wait_with_output() else {
        return (Vec::new(), None);
    };
    if !output.status.success() {
        return (Vec::new(), None);
    }
    let rows = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let rss_kib = parts.next()?.parse::<u64>().ok()?;
            Some((pid, ppid, rss_kib.saturating_mul(1024)))
        })
        .collect();
    (rows, Some(ps_pid))
}

/// 이 프로세스의 스레드 수 (macOS: proc_pidinfo PROC_PIDTASKINFO).
#[cfg(target_os = "macos")]
fn thread_count() -> Option<usize> {
    own_task_info().map(|info| info.pti_threadnum.max(0) as usize)
}

#[cfg(not(target_os = "macos"))]
fn thread_count() -> Option<usize> {
    None
}

// ---------------------------------------------------------------------------
// 프레임 기록
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct FrameRecord {
    ui_ms: f32,
    rows_rebuilt: usize,
    rows_painted: usize,
    shapes: usize,
    dirty_rows: usize,
    alloc_count: Option<u64>,
    alloc_bytes: Option<u64>,
    /// causes 배열의 인덱스 (문자열 중복 저장 회피 — 프레임당 alloc 0)
    cause: u32,
}

struct FrameBegin {
    at: Instant,
    alloc: Option<(u64, u64)>,
    cause: u32,
}

// ---------------------------------------------------------------------------
// 샘플러 스레드와 공유 상태
// ---------------------------------------------------------------------------

struct Shared {
    /// 현재 살아있는 워크스페이스 수 (active 1 + warm) — UI 스레드가 갱신.
    workspaces: AtomicUsize,
    /// 종료 요청됨 (샘플러가 deadline에서 set).
    closing: AtomicBool,
    /// 샘플러의 최신 RSS 샘플 — ws_step 등 고빈도 이벤트가 UI 스레드에서 `ps`를 부르지
    /// 않도록 캐시로 쓴다 (최대 2초 stale — 보고서에 명시).
    last_rss: std::sync::Mutex<Option<RssSample>>,
}

// ---------------------------------------------------------------------------
// 벤치 드라이버
// ---------------------------------------------------------------------------

/// createdelete 시나리오의 반복 단계.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CdPhase {
    Create,
    WaitSpawn,
    Delete,
}

/// 워크스페이스 생성 burst 계측 상태 (ws_step).
struct WsBurst {
    spawned_at: Instant,
    first_snapshot: bool,
    first_render: bool,
    stable_since: Option<Instant>,
    done: bool,
}

pub struct Bench {
    pub opts: BenchOptions,
    log: Arc<BenchLog>,
    shared: Arc<Shared>,
    ctx: egui::Context,

    // 프레임 계측
    frame_begin: Option<FrameBegin>,
    frames: Vec<FrameRecord>,
    causes: Vec<String>,
    first_frame_seen: bool,

    // 시나리오 상태
    setup_done: bool,
    /// 앱이 시작한 기준 워크스페이스 — createdelete가 삭제 전에 여기로 물러난다
    /// (활성 워크스페이스는 삭제할 수 없다는 앱 규칙 때문).
    pub base: Option<String>,
    /// switch 시나리오의 다음 전환 시각
    next_switch: Instant,
    // createdelete
    cd_phase: CdPhase,
    cd_iter: usize,
    cd_wait_until: Instant,
    cd_current: Option<String>,
    cd_delete_logged: bool,
    burst: Option<WsBurst>,
    finished: bool,
}

/// 시나리오별 셸이 뜨고 첫 화면이 안정될 때까지의 대기 (createdelete 반복 간격).
const CD_SPAWN_WAIT: Duration = Duration::from_millis(150);
/// switch 시나리오의 전환 간격.
const SWITCH_INTERVAL: Duration = Duration::from_millis(400);

impl Bench {
    /// env가 없으면 None — 여기서 끝난다(기본 실행 경로 불변).
    /// `log`는 main이 프로세스 시작 시점에 만들어 `start` 스테이지를 이미 찍은 것.
    pub fn from_env(log: Arc<BenchLog>, ctx: egui::Context) -> Option<Self> {
        let opts = BenchOptions::from_env()?;
        let shared = Arc::new(Shared {
            workspaces: AtomicUsize::new(1),
            closing: AtomicBool::new(false),
            last_rss: std::sync::Mutex::new(None),
        });
        spawn_sampler(&opts, Arc::clone(&log), Arc::clone(&shared), ctx.clone());
        let now = Instant::now();
        Some(Self {
            frames: Vec::with_capacity((opts.secs as usize + 1) * 120),
            causes: vec!["none".to_owned()],
            opts,
            log,
            shared,
            ctx,
            frame_begin: None,
            first_frame_seen: false,
            setup_done: false,
            base: None,
            next_switch: now + SWITCH_INTERVAL,
            cd_phase: CdPhase::Create,
            cd_iter: 0,
            cd_wait_until: now,
            cd_current: None,
            cd_delete_logged: false,
            burst: None,
            finished: false,
        })
    }

    pub fn scenario(&self) -> Scenario {
        self.opts.scenario
    }

    pub fn set_workspaces(&self, count: usize) {
        self.shared.workspaces.store(count, Ordering::Relaxed);
    }

    /// 샘플러가 deadline에 도달했는가 — App은 이때 남은 정리를 하고 창을 닫는다.
    pub fn closing(&self) -> bool {
        self.shared.closing.load(Ordering::Relaxed)
    }

    // --- 프레임 계측 -------------------------------------------------------

    pub fn frame_begin(&mut self, ctx: &egui::Context) {
        // cause 문자열화는 카운터 스냅샷 **이전에** — 이 alloc이 프레임 델타에 섞이지 않게.
        let cause = self.intern_cause(ctx);
        self.frame_begin = Some(FrameBegin {
            alloc: self.opts.alloc_stats.then(thread_alloc_stats).flatten(),
            cause,
            at: Instant::now(),
        });
    }

    fn intern_cause(&mut self, ctx: &egui::Context) -> u32 {
        let causes = ctx.repaint_causes();
        let Some(first) = causes.first() else {
            return 0; // "none"
        };
        // reason이 빈 경우가 많다 — 후행 공백을 남기지 않는다.
        let text = format!("{}:{} {}", first.file, first.line, first.reason)
            .trim_end()
            .to_owned();
        if let Some(index) = self.causes.iter().position(|c| c == &text) {
            return index as u32;
        }
        self.causes.push(text);
        (self.causes.len() - 1) as u32
    }

    pub fn frame_end(&mut self, counters: terminal::renderer_egui::RenderCounters) {
        let Some(begin) = self.frame_begin.take() else {
            return;
        };
        let ui_ms = begin.at.elapsed().as_secs_f32() * 1000.0;
        let (alloc_count, alloc_bytes) = match (begin.alloc, thread_alloc_stats()) {
            (Some((c0, b0)), Some((c1, b1))) => {
                (Some(c1.saturating_sub(c0)), Some(b1.saturating_sub(b0)))
            }
            _ => (None, None),
        };
        self.frames.push(FrameRecord {
            ui_ms,
            rows_rebuilt: counters.rows_rebuilt,
            rows_painted: counters.rows_painted,
            shapes: counters.shapes,
            dirty_rows: counters.dirty_rows,
            alloc_count,
            alloc_bytes,
            cause: begin.cause,
        });
        // 워크스페이스 생성 burst의 first_render / stable 단계 판정.
        if let Some(burst) = self.burst.as_mut()
            && !burst.done
        {
            if !burst.first_render && counters.rows_painted > 0 {
                burst.first_render = true;
                let ms = burst.spawned_at.elapsed().as_secs_f64() * 1000.0;
                emit_ws_step(&self.log, &self.shared, "first_render", ms);
            }
            if burst.first_render {
                if counters.dirty_rows == 0 {
                    let since = *burst.stable_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(500) {
                        burst.done = true;
                        let ms = burst.spawned_at.elapsed().as_secs_f64() * 1000.0;
                        emit_ws_step(&self.log, &self.shared, "stable", ms);
                    }
                } else {
                    burst.stable_since = None;
                }
            }
        }
        if !self.first_frame_seen {
            self.first_frame_seen = true;
            self.emit_rss_stage("first_frame");
        }
    }

    /// 첫 스냅샷 도착(ws_step) — App이 활성 workspace_ui를 보고 알려준다.
    pub fn note_first_snapshot(&mut self) {
        if let Some(burst) = self.burst.as_mut()
            && !burst.first_snapshot
        {
            burst.first_snapshot = true;
            let ms = burst.spawned_at.elapsed().as_secs_f64() * 1000.0;
            emit_ws_step(&self.log, &self.shared, "first_snapshot", ms);
        }
    }

    pub fn begin_burst(&mut self) {
        self.burst = Some(WsBurst {
            spawned_at: Instant::now(),
            first_snapshot: false,
            first_render: false,
            stable_since: None,
            done: false,
        });
    }

    // --- 이벤트 -----------------------------------------------------------

    /// `ps` 동기 호출을 포함한다 — 드문 스테이지에서만 쓴다(프레임 루프 금지).
    pub fn emit_rss_stage(&self, stage: &str) {
        let sample = sample_rss();
        if let Ok(mut slot) = self.shared.last_rss.lock() {
            *slot = Some(sample);
        }
        emit_rss(
            &self.log,
            stage,
            sample,
            self.shared.workspaces.load(Ordering::Relaxed),
        );
    }

    pub fn emit_ws_step(&self, step: &str, ms: f64) {
        emit_ws_step(&self.log, &self.shared, step, ms);
    }

    /// GPU 리소스 — **egui가 요청한 텍스처 descriptor 기준 추정치**.
    /// wgpu/glow 모두 실제 GPU 할당량을 노출하지 않으므로 buffers/upload_bytes는 넣지 않는다
    /// (UNKNOWN — 보고서 참조). 지어내지 않는다.
    pub fn emit_gpu(&self) {
        let manager = self.ctx.tex_manager();
        let manager = manager.read();
        let textures = manager.num_allocated();
        let texture_bytes: usize = manager
            .allocated()
            .map(|(_, meta)| meta.size[0] * meta.size[1] * meta.bytes_per_pixel)
            .sum();
        let mut map = fields! { "textures": textures, "texture_bytes": texture_bytes };
        // 폰트 아틀라스는 항상 TextureId::default() (= Managed(0)).
        if let Some(meta) = manager.meta(egui::TextureId::default()) {
            map.insert(
                "atlas_px".to_owned(),
                serde_json::json!(format!("{}x{}", meta.size[0], meta.size[1])),
            );
        }
        self.log.emit("gpu", map);
    }

    /// 시나리오 종료 — 프레임 버퍼를 flush하고 요약을 낸다. on_exit에서 1회.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.emit_gpu();

        for frame in &self.frames {
            let mut map = fields! {
                "ui_ms": frame.ui_ms,
                "rows_rebuilt": frame.rows_rebuilt,
                "rows_painted": frame.rows_painted,
                "shapes": frame.shapes,
                "dirty_rows": frame.dirty_rows,
                "cause": self.causes.get(frame.cause as usize).map(String::as_str).unwrap_or("none"),
            };
            // feature 없이는 null — 0으로 위조하지 않는다.
            map.insert(
                "alloc_count".to_owned(),
                serde_json::json!(frame.alloc_count),
            );
            map.insert(
                "alloc_bytes".to_owned(),
                serde_json::json!(frame.alloc_bytes),
            );
            self.log.emit("frame", map);
        }

        let mut ui_ms: Vec<f32> = self.frames.iter().map(|f| f.ui_ms).collect();
        self.log.emit(
            "frame_summary",
            fields! {
                "scenario": self.opts.scenario.as_str(),
                "count": ui_ms.len(),
                "p50": crate::perf::percentile(&mut ui_ms, 0.50),
                "p95": crate::perf::percentile(&mut ui_ms, 0.95),
                "p99": crate::perf::percentile(&mut ui_ms, 0.99),
                "max": crate::perf::percentile(&mut ui_ms, 1.0),
                // 프레임 = repaint 1회 (eframe은 repaint가 요청된 프레임에만 ui()를 돈다)
                "repaints": self.frames.len(),
            },
        );

        if self.opts.alloc_stats {
            let mut counts: Vec<f32> = self
                .frames
                .iter()
                .filter_map(|f| f.alloc_count.map(|v| v as f32))
                .collect();
            let mut bytes: Vec<f32> = self
                .frames
                .iter()
                .filter_map(|f| f.alloc_bytes.map(|v| v as f32))
                .collect();
            self.log.emit(
                "alloc_summary",
                fields! {
                    "scenario": self.opts.scenario.as_str(),
                    "frames": counts.len(),
                    "alloc_per_frame_p50": crate::perf::percentile(&mut counts, 0.50) as u64,
                    "bytes_per_frame_p50": crate::perf::percentile(&mut bytes, 0.50) as u64,
                },
            );
        }
    }

    // --- 시나리오 드라이버 상태 -------------------------------------------

    pub fn needs_setup(&mut self) -> bool {
        if self.setup_done || !self.opts.driver {
            return false;
        }
        self.setup_done = true;
        true
    }

    /// switch 시나리오: 전환할 때가 됐는가.
    pub fn switch_due(&mut self, now: Instant) -> bool {
        if !self.opts.driver || self.opts.scenario != Scenario::Switch || now < self.next_switch {
            return false;
        }
        self.next_switch = now + SWITCH_INTERVAL;
        true
    }

    /// createdelete 시나리오의 다음 단계. None이면 할 일 없음(반복 종료 포함).
    pub fn createdelete_step(&mut self, now: Instant) -> Option<CreateDeleteStep> {
        if !self.opts.driver || self.opts.scenario != Scenario::CreateDelete {
            return None;
        }
        if self.cd_iter >= self.opts.iters {
            if !self.cd_delete_logged {
                self.cd_delete_logged = true;
                self.emit_rss_stage("ws_delete_done");
            }
            return None;
        }
        match self.cd_phase {
            CdPhase::Create => {
                self.cd_phase = CdPhase::WaitSpawn;
                self.cd_wait_until = now + CD_SPAWN_WAIT;
                Some(CreateDeleteStep::Create(self.cd_iter))
            }
            CdPhase::WaitSpawn => {
                if now < self.cd_wait_until {
                    return None;
                }
                self.cd_phase = CdPhase::Delete;
                None
            }
            CdPhase::Delete => {
                self.cd_phase = CdPhase::Create;
                self.cd_iter += 1;
                self.cd_current.take().map(CreateDeleteStep::Delete)
            }
        }
    }

    pub fn set_createdelete_current(&mut self, id: String) {
        self.cd_current = Some(id);
    }

    fn createdelete_done(&self) -> bool {
        self.cd_iter >= self.opts.iters
    }

    /// 이 시나리오가 **드라이버 동작을 위해** 계속 프레임을 요구하는가.
    /// idle/dirty1/bulk/fullscreen은 false — 셸 출력이 있을 때만 repaint되고,
    /// idle은 repaint 0이 유지된다(측정 전제).
    pub fn needs_frames(&self) -> bool {
        if !self.opts.driver {
            return false;
        }
        match self.opts.scenario {
            Scenario::Switch => true,
            Scenario::CreateDelete => !self.createdelete_done(),
            _ => false,
        }
    }
}

pub enum CreateDeleteStep {
    /// n번째 반복 — 워크스페이스를 만들고 전환하고 셸을 띄운다
    Create(usize),
    /// 이 워크스페이스를 지운다 (활성에서 물러난 뒤)
    Delete(String),
}

fn emit_rss(log: &BenchLog, stage: &str, sample: RssSample, workspaces: usize) {
    let mut map = fields! {
        "stage": stage,
        "app_bytes": sample.app_bytes,
        "child_bytes": sample.child_bytes,
        "total_bytes": sample.app_bytes.saturating_add(sample.child_bytes),
        "workspaces": workspaces,
    };
    // 스레드 수를 못 얻는 플랫폼에서는 필드를 생략한다(0 위조 금지).
    if let Some(threads) = sample.threads {
        map.insert("threads".to_owned(), serde_json::json!(threads));
    }
    log.emit("rss", map);
}

/// ws_step의 rss/threads는 **샘플러의 최신 캐시**(≤2s stale)를 쓴다 — 100회 반복에서
/// 단계마다 `ps`를 동기 호출하면 그 비용이 측정을 오염시킨다.
fn emit_ws_step(log: &BenchLog, shared: &Shared, step: &str, ms: f64) {
    let cached = shared.last_rss.lock().ok().and_then(|slot| *slot);
    let mut map = fields! { "step": step, "ms": ms };
    if let Some(sample) = cached {
        map.insert(
            "rss".to_owned(),
            serde_json::json!(sample.app_bytes.saturating_add(sample.child_bytes)),
        );
        if let Some(threads) = sample.threads {
            map.insert("threads".to_owned(), serde_json::json!(threads));
        }
    }
    log.emit("ws_step", map);
}

/// 주기 샘플러 + 종료 타이머. **UI 스레드 밖**이라 idle 시나리오에서 repaint를 만들지
/// 않는다 (idle repaint 0 측정의 전제). deadline에만 창 닫기 명령을 보낸다.
fn spawn_sampler(opts: &BenchOptions, log: Arc<BenchLog>, shared: Arc<Shared>, ctx: egui::Context) {
    let driver = opts.driver;
    let resource_stats = opts.resource_stats;
    let deadline = driver.then(|| Instant::now() + Duration::from_secs(opts.secs));
    std::thread::Builder::new()
        .name("deppy-bench-sampler".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let mut stable_5s_done = false;
            let mut after_30s_done = false;
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let now = Instant::now();
                let sample = sample_rss();
                if let Ok(mut slot) = shared.last_rss.lock() {
                    *slot = Some(sample);
                }
                let workspaces = shared.workspaces.load(Ordering::Relaxed);

                // 2초 주기 periodic (DEPPY_RESOURCE_STATS=1)
                if resource_stats && now.duration_since(started).as_millis() % 2000 < 250 {
                    emit_rss(&log, "periodic", sample, workspaces);
                }
                if !stable_5s_done && now.duration_since(started) >= Duration::from_secs(5) {
                    stable_5s_done = true;
                    emit_rss(&log, "stable_5s", sample, workspaces);
                }
                if !after_30s_done && now.duration_since(started) >= Duration::from_secs(30) {
                    after_30s_done = true;
                    emit_rss(&log, "after_30s", sample, workspaces);
                }
                if let Some(deadline) = deadline
                    && now >= deadline
                {
                    emit_rss(&log, "scenario_end", sample, workspaces);
                    shared.closing.store(true, Ordering::Release);
                    // 앱이 스스로 정상 종료한다 — on_exit 경로를 타야 프로세스/스레드
                    // 잔존 검증이 가능하다.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                    return;
                }
            }
        })
        .ok();
}

/// main이 프로세스 시작 직후 부른다 — env가 없으면 None(비용 0).
pub fn init_log() -> Option<Arc<BenchLog>> {
    let opts = BenchOptions::from_env()?;
    let log = Arc::new(BenchLog::new(opts.out.as_deref()));
    let sample = sample_rss();
    emit_rss(&log, "start", sample, 0);
    Some(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 시나리오_파싱과_명령() {
        assert_eq!(Scenario::parse("bulk"), Some(Scenario::Bulk));
        assert_eq!(Scenario::parse("nope"), None);
        // idle은 출력이 0이어야 한다 — 프롬프트조차 없는 sleep
        let (_, args) = Scenario::Idle.command().unwrap();
        assert!(args.join(" ").contains("sleep 86400"));
        // dirty1은 \r로 같은 행만 덮어쓴다
        assert!(
            Scenario::Dirty1
                .command()
                .unwrap()
                .1
                .join(" ")
                .contains("\\rprogress")
        );
        // bulk는 20만 줄
        assert!(
            Scenario::Bulk
                .command()
                .unwrap()
                .1
                .join(" ")
                .contains("head -n 200000")
        );
        // fullscreen은 clear 반복
        assert!(
            Scenario::Fullscreen
                .command()
                .unwrap()
                .1
                .join(" ")
                .contains("clear")
        );
    }

    #[test]
    fn rss_샘플은_앱과_자식을_분리한다() {
        let sample = sample_rss();
        // 자기 자신은 반드시 잡힌다 (ps가 있는 플랫폼)
        assert!(sample.app_bytes > 0, "app RSS를 못 읽었다");
        // 스레드 수는 macOS에서 얻을 수 있어야 한다
        #[cfg(target_os = "macos")]
        assert!(sample.threads.unwrap_or(0) > 0, "스레드 수를 못 읽었다");
    }

    #[test]
    fn jsonl은_t와_ev를_붙인다() {
        let dir = std::env::temp_dir().join(format!("deppy-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("b.jsonl");
        let log = BenchLog::new(Some(&path));
        log.emit("frame", fields! { "ui_ms": 1.5 });
        drop(log);
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["ev"], "frame");
        assert_eq!(value["ui_ms"], 1.5);
        assert!(value["t"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
