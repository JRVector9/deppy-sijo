mod agent_actions;
mod agent_detect;
mod agent_detect_worker;
mod agent_hooks;
mod agent_launcher;
mod agent_model_catalog;
mod agent_resume;
mod agent_session;
mod agent_shim;
mod agent_state_worker;
mod agent_surface;
mod agent_transcript;
mod agent_work_git;
mod alloc;
mod app;
mod bench;
mod claude_usage;
mod codex_app_server;
mod codex_backend_usage;
mod config;
mod dotenv_sync;
mod env;
mod env_reload;
mod fleet;
mod fonts;
mod git_cli;
mod kimi_usage;
mod lazy_worker;
mod llm_proxy;
mod local_llm;
mod logging;
mod mcp_import;
mod mem_pressure_monitor;
mod native_key_monitor;
mod notice_translate;
mod panic_policy;
mod paths;
mod perf;
#[allow(dead_code)]
mod port_inventory;
mod process_storm;
mod prompt_library;
mod pty_effort;
mod shortcuts;
mod status_feed;
mod tailscale;
mod theme;
mod ui;
mod worktree;

use std::path::{Path, PathBuf};

fn main() -> anyhow::Result<()> {
    // Install before paths, logging, config, or any worker can panic. Payloads may contain user
    // data; early static diagnostics can be dropped before tracing is ready, but never exposed.
    panic_policy::install_sanitized_panic_hook();
    // 스크롤백 해제(hidden/exited 전환) 시 mimalloc이 붙잡은 페이지를 OS로 반환하도록
    // runtime에 purge 훅을 건다. worker 스레드가 스크롤백을 해제한 직후 이 훅을 부른다
    // (mimalloc은 명시적 purge 없이는 자동 반환하지 않음 — 실측). worker보다 먼저 등록.
    runtime::set_memory_release_hook(alloc::purge);
    // 렌더러 A/B 실측(B1) — env 미설정이면 bench_log는 None이고 아래 경로는 전부 무시된다.
    // `start` 스테이지 RSS는 이 시점(창/렌더러 생성 전)에 이미 찍힌다.
    let bench_log = bench::init_log();
    let process_start = std::time::Instant::now();
    // 벤치 모드는 **사용자 실데이터를 오염시키지 않는다** — 임시 data/config dir로 격리한다.
    // (DEPPY_RESOURCE_STATS만 켠 경우는 실제 앱 관찰이 목적이라 격리하지 않는다.)
    let bench_root = bench::isolate_data_dir().then(bench_temp_root);
    let paths = match &bench_root {
        Some(root) => bench_paths(root)?,
        None => paths::AppPaths::init()?,
    };
    // guard가 drop되면 파일 로그 flush가 끊기므로 main 끝까지 유지한다.
    let (_log_guard, _log_stats) = init_logging(&paths)?;
    // 중복 실행 방지 lock (설계문서 PR-14 crash recovery). config 로드/생성보다
    // 먼저 잡는다 — 두 인스턴스의 config I/O 경쟁도 이 lock이 보호한다 (codex 리뷰).
    // drop 시 자동 해제되므로 main 끝까지 살려 둔다.
    let _run_lock = persist::LockFile::acquire(&paths.data_dir.join("deppy.lock"))
        .map_err(|e| anyhow::anyhow!("이미 실행 중이거나 lock 획득 실패: {e:#}"))?;

    let config = config::Config::load_or_create(&paths.config_dir)?;
    let config_path = config::config_path(&paths.config_dir);
    let data_dir = paths.data_dir.clone();
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        data_dir = %paths.data_dir.display(),
        log_dir = %paths.log_dir.display(),
        "앱 시작"
    );
    if let Some(log) = &bench_log {
        log.emit_rss_stage("pre_run_native", 0);
    }

    let result = eframe::run_native(
        "Deppy Sijo",
        eframe::NativeOptions {
            // production은 Wgpu/Metal 전용. A/B 벤치 빌드(`render-glow`)에서만
            // DEPPY_RENDERER=glow 선택을 허용한다.
            renderer: select_renderer(),
            // 창 위치/크기 영속 안 함 — 외부 모니터 분리 후 저장된 좌표로 복원되면
            // 창이 화면 밖에 떠서 "앱이 죽은 것처럼" 보인다 (2026-07-05 실증:
            // eframe 기본 복원은 현재 모니터 배치로 clamp되지 않았다). 위치 기억보다
            // 항상 보이는 것이 우선. 런타임 분리는 app.rs 오프스크린 감지가 방어.
            persist_window: false,
            // 주 화면 중앙에 뜬다 — 위치 미지정이면 OS가 임의(보조 모니터 포함) 배치해
            // 사용자가 창을 잃어버릴 수 있다 (2026-07-05: 왼쪽 외부 모니터에 떠서
            // "앱이 죽은 줄" — 실은 정상 실행 중이었다).
            centered: true,
            // 상단 바를 macOS 네이티브 타이틀바 영역으로 끌어올린다 (2026-07-06):
            // fullsize content view로 콘텐츠가 타이틀바까지 확장되고, 네이티브 제목
            // 텍스트는 숨긴다. 신호등(닫기/최소화/전체화면)은 그대로 남는다.
            // 상단 바 렌더는 신호등 폭만큼 왼쪽 여백을 두고, 빈 영역은 창 드래그로 처리한다.
            // eframe 문서 권장 조합: fullsize_content_view는 titlebar_shown(false)·
            // title_shown(false)와 함께 써야 한다. titlebar_shown(true)면 네이티브
            // 타이틀바가 상단 바를 덮어 잘라냈다(2026-07-06 사용자 화면). 신호등은
            // 데코레이션이 켜져 있어 그대로 남는다.
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1200.0, 800.0])
                .with_fullsize_content_view(true)
                .with_title_shown(false)
                .with_titlebar_shown(false),
            ..Default::default()
        },
        Box::new(move |cc| {
            // 저장된 테마를 시작 시점에 적용
            // 목업 팔레트(시안 액센트 + 쿨그레이)를 테마별로 심는다 — set_theme보다 먼저
            // 등록해야 프리퍼런스 적용 시 커스텀 색이 선택된다.
            theme::install_palette(&cc.egui_ctx);
            cc.egui_ctx.set_theme(config.ui.theme.to_egui());
            disable_egui_debug_warnings(&cc.egui_ctx);
            fonts::install_cjk_fallback(
                &cc.egui_ctx,
                config.ui.ui_font.as_deref(),
                &config.terminal.mono_font,
                &config.terminal.mono_weight,
            );
            install_macos_menu();
            // 커스텀 상단바(TOP_BAR_HEIGHT)가 macOS 기본 타이틀바(~28pt)보다 높아
            // 신호등이 위로 치우쳐 보였다 — 상단바 높이 기준으로 수직 중앙 재배치
            // (2026-07-25 사용자, winit 신규 API — third_party/winit-0.30.13 patch).
            #[cfg(target_os = "macos")]
            if let Some(window) = cc.winit_window() {
                use winit::platform::macos::WindowExtMacOS as _;
                window.set_traffic_light_titlebar_height(app::TOP_BAR_HEIGHT as f64);
            }
            native_key_monitor::install();
            // 메모리 압박 감지 (로드맵 C1) — 신호만 설치, 소비는 logic()에서.
            mem_pressure_monitor::install(cc.egui_ctx.clone());
            // B1: 실제로 초기화된 백엔드/어댑터를 기록한다 (요청값이 아니라 결과값).
            let bench = bench_log.and_then(|log| {
                log.emit("renderer", renderer_fields(cc, process_start));
                bench::Bench::from_env(log, cc.egui_ctx.clone())
            });
            if let Some(bench) = &bench {
                bench.emit_rss_stage("renderer_init");
            }
            Ok(Box::new(app::App::bootstrap(
                config,
                config_path,
                data_dir,
                cc.egui_ctx.clone(),
                bench,
            )?))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe 실행 실패: {e}"));

    // 벤치 임시 data dir 정리 — 사용자 실데이터와 격리된 디렉터리만 지운다.
    if let Some(root) = bench_root {
        let _ = std::fs::remove_dir_all(root);
    }
    result
}

/// production은 항상 Wgpu. `DEPPY_RENDERER=glow`는 `render-glow` 벤치 feature에서만 유효.
fn select_renderer() -> eframe::Renderer {
    match std::env::var("DEPPY_RENDERER").as_deref() {
        #[cfg(feature = "render-glow")]
        Ok("glow") => eframe::Renderer::Glow,
        #[cfg(not(feature = "render-glow"))]
        Ok("glow") => {
            eprintln!("DEPPY_RENDERER=glow는 render-glow 벤치 빌드에서만 지원 — wgpu 사용");
            eframe::Renderer::Wgpu
        }
        Ok("wgpu") => eframe::Renderer::Wgpu,
        Ok(other) => {
            eprintln!("DEPPY_RENDERER='{other}' 알 수 없음 — wgpu 사용");
            eframe::Renderer::Wgpu
        }
        Err(_) => eframe::Renderer::Wgpu,
    }
}

/// 실제 초기화된 렌더 백엔드 정보. wgpu면 adapter/backend(Metal 여부)를 어댑터에서 직접
/// 읽고, glow면 GL 컨텍스트에서 GL_RENDERER/GL_VERSION을 읽는다 — 둘 다 **실측값**이다.
fn renderer_fields(
    cc: &eframe::CreationContext<'_>,
    process_start: std::time::Instant,
) -> serde_json::Map<String, serde_json::Value> {
    let init_ms = process_start.elapsed().as_secs_f64() * 1000.0;
    let (backend, adapter, gpu_backend) = match cc.wgpu_render_state.as_ref() {
        Some(state) => {
            let info = state.adapter.get_info();
            ("wgpu", info.name, format!("{:?}", info.backend))
        }
        None => {
            let (renderer, version) = gl_strings(cc);
            ("glow", renderer, version)
        }
    };
    let mut map = serde_json::Map::new();
    map.insert("backend".to_owned(), backend.into());
    map.insert("adapter".to_owned(), adapter.into());
    map.insert("gpu_backend".to_owned(), gpu_backend.into());
    map.insert("init_ms".to_owned(), init_ms.into());
    map
}

/// glow 경로의 (GL_RENDERER, GL_VERSION). 컨텍스트가 없으면 "unknown".
#[cfg(feature = "render-glow")]
fn gl_strings(cc: &eframe::CreationContext<'_>) -> (String, String) {
    use eframe::glow::HasContext as _;
    let Some(gl) = cc.gl.as_ref() else {
        return ("unknown".to_owned(), "unknown".to_owned());
    };
    // SAFETY: eframe이 이 스레드에서 GL 컨텍스트를 current로 만든 뒤 콜백을 부른다.
    unsafe {
        (
            gl.get_parameter_string(eframe::glow::RENDERER),
            gl.get_parameter_string(eframe::glow::VERSION),
        )
    }
}

#[cfg(not(feature = "render-glow"))]
fn gl_strings(_cc: &eframe::CreationContext<'_>) -> (String, String) {
    ("unavailable".to_owned(), "render-glow disabled".to_owned())
}

/// 벤치 전용 임시 루트 (pid로 구분 — 동시 실행/실데이터 오염 방지).
fn bench_temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("deppy-bench-{}", std::process::id()))
}

fn bench_paths(root: &Path) -> anyhow::Result<paths::AppPaths> {
    let paths = paths::AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        log_dir: root.join("data").join("logs"),
    };
    for dir in [&paths.config_dir, &paths.data_dir, &paths.log_dir] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(paths)
}

/// macOS 네이티브 메뉴바 (2026-07-05 사용자 요청) — About/설정(⌘,)/종료(⌘Q).
/// 이벤트는 app.rs가 MenuEvent::receiver로 폴링한다 ("settings" id).
#[cfg(target_os = "macos")]
fn install_macos_menu() {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    let menu = muda::Menu::new();
    let app_menu = muda::Submenu::new("Deppy Sijo", true);
    let about = muda::PredefinedMenuItem::about(
        Some("Deppy Sijo에 관하여"),
        Some(muda::AboutMetadata {
            name: Some("Deppy Sijo".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        }),
    );
    let settings = muda::MenuItem::with_id(
        "settings",
        "설정…",
        true,
        Some(Accelerator::new(Some(Modifiers::META), Code::Comma)),
    );
    let quit = muda::PredefinedMenuItem::quit(Some("Deppy Sijo 종료"));
    let items: [&dyn muda::IsMenuItem; 5] = [
        &about,
        &muda::PredefinedMenuItem::separator(),
        &settings,
        &muda::PredefinedMenuItem::separator(),
        &quit,
    ];
    if let Err(e) = app_menu.append_items(&items) {
        tracing::warn!("메뉴 구성 실패: {e}");
        return;
    }
    if let Err(e) = menu.append(&app_menu) {
        tracing::warn!("메뉴 구성 실패: {e}");
        return;
    }
    menu.init_for_nsapp();
    // 메뉴는 앱 수명 내내 유지 — drop되면 NSMenu 항목이 사라진다
    std::mem::forget(menu);
    std::mem::forget(app_menu);
}

#[cfg(not(target_os = "macos"))]
fn install_macos_menu() {}

/// egui 디버그 빌드 전용 화면 경고(빨간 표시 계열)를 끈다 — dev-run 디버그 빌드를
/// 일상 사용하는 앱이라 개발용 경고가 사용자에게 그대로 노출된다. 릴리스 빌드는
/// 원래 안 그리므로 이 설정으로 디버그/릴리스 화면이 같아진다.
/// 경고 자체를 조사할 때는 해당 줄을 잠시 주석 처리하고 재현하면 된다.
pub fn disable_egui_debug_warnings(ctx: &egui::Context) {
    // 위젯 ID 충돌 경고(error_fg_color 테두리 + 🔥 텍스트) — 파일 트리 토글·설정
    // 화면에서 깜빡였다(2026-07-18 보고, 1faeb30).
    ctx.options_mut(|options| options.warn_on_id_clash = false);
    // egui 0.35 신설 `Style.debug.warn_if_rect_changes_id`(디버그 빌드 기본 on)는
    // "직전 패스와 같은 rect에 전혀 다른 위젯 id가 오면" Color32::RED 2px 테두리를
    // 그린다. 파일 트리는 show_rows 가상화 + 경로 기반 행 id라, 스크롤이 정확히
    // 행높이 배수만큼 이동한 프레임마다 같은 rect에 다른 행(id)이 들어와 정상
    // 동작인데도 오발화한다(2026-07-18 "트리 스크롤 중 빨간 네모" 보고).
    // warn_on_id_clash(Options)와는 별개 플래그(Style.debug)라 위 설정으로는 안 꺼진다.
    // 다크/라이트 스타일 모두에 적용해야 런타임 테마 전환 후에도 유지된다.
    #[cfg(debug_assertions)]
    ctx.all_styles_mut(|style| style.debug.warn_if_rect_changes_id = false);
}

fn init_logging(
    paths: &paths::AppPaths,
) -> Result<
    (
        tracing_appender::non_blocking::WorkerGuard,
        logging::AppLogStats,
    ),
    logging::AppLogError,
> {
    let (sink, stats) = logging::open_app_log_sink(&paths.log_dir)?;
    let (file_writer, guard) = logging::spawn_non_blocking_app_logger(sink);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(file_writer)
        .with_ansi(false)
        .init();
    Ok((guard, stats))
}
