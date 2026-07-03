mod app;
mod config;
mod env;
mod fonts;
mod paths;
mod storage;
mod ui;

fn main() -> anyhow::Result<()> {
    let paths = paths::AppPaths::init()?;
    // guard가 drop되면 파일 로그 flush가 끊기므로 main 끝까지 유지한다.
    let _log_guard = init_logging(&paths);
    let config = config::Config::load_or_create(&paths.config_dir)?;
    let config_path = config::config_path(&paths.config_dir);
    // insecure fallback 금지(설계문서 1.4) — 등록 실패 시 credential 조작이 에러로 표면화된다
    if let Err(e) = secret::init_platform_store() {
        tracing::warn!("keyring store 초기화 실패 — 자격증명 기능 비활성: {e:#}");
    }
    let db = storage::Db::open(&paths.data_dir.join("metadata.sqlite3"))?;
    let workspace_id = db.ensure_default_workspace()?;
    // 세션 로그 루트 (설계문서 7장: logs/<workspace_id>/<session_id>/)
    let logs_root = paths.data_dir.join("logs").join(&workspace_id);
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        data_dir = %paths.data_dir.display(),
        log_dir = %paths.log_dir.display(),
        "앱 시작"
    );

    eframe::run_native(
        "Deppy Sijo",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            // 저장된 테마를 시작 시점에 적용
            cc.egui_ctx.set_theme(config.ui.theme.to_egui());
            fonts::install_cjk_fallback(&cc.egui_ctx);
            Ok(Box::new(app::App::new(
                config,
                config_path,
                db,
                workspace_id,
                logs_root,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe 실행 실패: {e}"))
}

fn init_logging(paths: &paths::AppPaths) -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(&paths.log_dir, "app.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(file_writer)
        .with_ansi(false)
        .init();
    guard
}
