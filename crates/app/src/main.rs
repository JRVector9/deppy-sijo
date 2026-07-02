mod app;
mod config;
mod paths;

fn main() -> anyhow::Result<()> {
    let paths = paths::AppPaths::init()?;
    // guard가 drop되면 파일 로그 flush가 끊기므로 main 끝까지 유지한다.
    let _log_guard = init_logging(&paths);
    let config = config::Config::load_or_create(&paths.config_dir)?;
    tracing::info!(
        config_dir = %paths.config_dir.display(),
        data_dir = %paths.data_dir.display(),
        log_dir = %paths.log_dir.display(),
        "앱 시작"
    );

    eframe::run_native(
        "Deppy Jelly",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app::App::new(config)))),
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
